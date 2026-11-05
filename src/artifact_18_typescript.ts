import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { DeltaNeutralRouter } from "../target/types/delta_neutral_router";
import {
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { Keypair, PublicKey } from "@solana/web3.js";
import { expect } from "chai";

describe("Aegis Delta-Neutral Yield Router Tests", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.DeltaNeutralRouter as Program<DeltaNeutralRouter>;

  const admin = (provider.wallet as anchor.Wallet).payer;
  const keeper = Keypair.generate();
  const alice = Keypair.generate();

  let baseMint: PublicKey;
  let targetMint: PublicKey;
  let vaultPda: PublicKey;
  let vaultBump: number;
  let shareMintPda: PublicKey;
  let baseVaultPda: PublicKey;

  let aliceBaseAccount: PublicKey;
  let aliceShareAccount: PublicKey;

  before(async () => {
    // Airdrop SOL to test actors
    const txAirdropAlice = await provider.connection.requestAirdrop(alice.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
    const txAirdropKeeper = await provider.connection.requestAirdrop(keeper.publicKey, 5 * anchor.web3.LAMPORTS_PER_SOL);
    await provider.connection.confirmTransaction(txAirdropAlice);
    await provider.connection.confirmTransaction(txAirdropKeeper);

    // Create Mock Base Mint (USDC, 6 decimals) and Target Mint (SOL, 9 decimals)
    baseMint = await createMint(provider.connection, admin, admin.publicKey, null, 6);
    targetMint = await createMint(provider.connection, admin, admin.publicKey, null, 9);

    // Derive PDAs
    [vaultPda, vaultBump] = PublicKey.findProgramAddressSync(
      [Buffer.from("aegis_vault"), baseMint.toBuffer()],
      program.programId
    );

    [shareMintPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("aegis_lp_mint"), vaultPda.toBuffer()],
      program.programId
    );

    [baseVaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("base_vault"), vaultPda.toBuffer()],
      program.programId
    );

    // Setup Alice Base Token Account with 100,000 USDC
    aliceBaseAccount = await createAccount(provider.connection, alice, baseMint, alice.publicKey);
    await mintTo(provider.connection, admin, baseMint, aliceBaseAccount, admin, 100_000 * 1e6);

    aliceShareAccount = await createAccount(provider.connection, alice, shareMintPda, alice.publicKey);
  });

  it("1. Initializes Vault with strict risk tolerances", async () => {
    const targetLtvBps = 6000; // 60%
    const maxLtvBps = 7500;    // 75%
    const driftToleranceBps = 200; // 2%

    await program.methods
      .initializeVault(targetLtvBps, maxLtvBps, driftToleranceBps)
      .accounts({
        admin: admin.publicKey,
        keeper: keeper.publicKey,
        baseMint: baseMint,
        targetMint: targetMint,
        vault: vaultPda,
        shareMint: shareMintPda,
        baseVault: baseVaultPda,
        systemProgram: anchor.web3.SystemProgram.programId,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    const vault = await program.account.vaultState.fetch(vaultPda);
    expect(vault.admin.toBase58()).to.equal(admin.publicKey.toBase58());
    expect(vault.keeper.toBase58()).to.equal(keeper.publicKey.toBase58());
    expect(vault.targetLtvBps).to.equal(targetLtvBps);
    expect(vault.maxLtvBps).to.equal(maxLtvBps);
    expect(vault.isPaused).to.be.false;
  });

  it("2. Accepts user deposits and mints 1:1 shares on genesis", async () => {
    const depositAmount = new anchor.BN(10_000 * 1e6); // 10,000 USDC

    await program.methods
      .deposit(depositAmount)
      .accounts({
        user: alice.publicKey,
        vault: vaultPda,
        baseVault: baseVaultPda,
        shareMint: shareMintPda,
        userBaseToken: aliceBaseAccount,
        userShareToken: aliceShareAccount,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
      })
      .signers([alice])
      .rpc();

    const vault = await program.account.vaultState.fetch(vaultPda);
    expect(vault.totalManagedBase.toNumber()).to.equal(depositAmount.toNumber());

    const aliceShares = await getAccount(provider.connection, aliceShareAccount);
    expect(Number(aliceShares.amount)).to.equal(depositAmount.toNumber());
  });

  it("3. Enforces LTV ceiling and blocks over-leveraged rebalancing", async () => {
    // Attempting to borrow SOL at 90% LTV should be rejected
    const mockMfiGroup = Keypair.generate().publicKey;
    const mockMfiAccount = Keypair.generate().publicKey;
    const mockBaseBank = Keypair.generate().publicKey;
    const mockBaseMfiVault = Keypair.generate().publicKey;
    const mockTargetBank = Keypair.generate().publicKey;
    const mockTargetMfiVault = Keypair.generate().publicKey;
    const mockKaminoMarket = Keypair.generate().publicKey;
    const mockKaminoReserve = Keypair.generate().publicKey;
    const mockKaminoSupply = Keypair.generate().publicKey;

    const vaultTargetAccount = await createAccount(
      provider.connection,
      admin,
      targetMint,
      vaultPda
    );

    const excessiveBorrowSol = new anchor.BN(60 * 1e9); // 60 SOL @ $150 = $9,000 against $10,000 (90% LTV)
    const solPriceScaled = new anchor.BN(150 * 1e6); // $150.00 scaled

    try {
      await program.methods
        .rebalanceRoute(new anchor.BN(0), excessiveBorrowSol, solPriceScaled)
        .accounts({
          keeper: keeper.publicKey,
          vault: vaultPda,
          baseVault: baseVaultPda,
          targetMint: targetMint,
          vaultTargetToken: vaultTargetAccount,
          marginfiProgram: program.programId, // Mock CPI target
          marginfiGroup: mockMfiGroup,
          marginfiAccount: mockMfiAccount,
          marginfiBaseBank: mockBaseBank,
          marginfiBaseVault: mockBaseMfiVault,
          marginfiTargetBank: mockTargetBank,
          marginfiTargetVault: mockTargetMfiVault,
          kaminoProgram: program.programId,
          kaminoMarket: mockKaminoMarket,
          kaminoReserve: mockKaminoReserve,
          kaminoLiquiditySupply: mockKaminoSupply,
          tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        })
        .signers([keeper])
        .rpc();

      expect.fail("Should have failed with LtvLimitExceeded");
    } catch (err: any) {
      expect(err.error?.errorCode?.code).to.equal("LtvLimitExceeded");
    }
  });

  it("4. Rejects unprivileged emergency pause calls", async () => {
    try {
      await program.methods
        .emergencyUnwind(true)
        .accounts({
          admin: alice.publicKey, // Not the authorized admin
          vault: vaultPda,
          vaultTargetToken: baseVaultPda,
          marginfiTargetVault: baseVaultPda,
          tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        })
        .signers([alice])
        .rpc();

      expect.fail("Should have rejected unauthorized caller");
    } catch (err: any) {
      expect(err.error?.errorCode?.code).to.equal("Unauthorized");
    }
  });

  it("5. Allows admin to trigger emergency circuit breaker and blocks actions", async () => {
    await program.methods
      .emergencyUnwind(true)
      .accounts({
        admin: admin.publicKey,
        vault: vaultPda,
        vaultTargetToken: baseVaultPda,
        marginfiTargetVault: baseVaultPda,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
      })
      .rpc();

    const vault = await program.account.vaultState.fetch(vaultPda);
    expect(vault.isPaused).to.be.true;

    // Deposit should now fail
    try {
      await program.methods
        .deposit(new anchor.BN(100 * 1e6))
        .accounts({
          user: alice.publicKey,
          vault: vaultPda,
          baseVault: baseVaultPda,
          shareMint: shareMintPda,
          userBaseToken: aliceBaseAccount,
          userShareToken: aliceShareAccount,
          tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        })
        .signers([alice])
        .rpc();

      expect.fail("Should have blocked deposit during paused state");
    } catch (err: any) {
      expect(err.error?.errorCode?.code).to.equal("VaultPaused");
    }
  });

  it("6. Allows proportional withdrawals upon unpausing", async () => {
    // Unpause vault
    await program.methods
      .emergencyUnwind(false)
      .accounts({
        admin: admin.publicKey,
        vault: vaultPda,
        vaultTargetToken: baseVaultPda,
        marginfiTargetVault: baseVaultPda,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
      })
      .rpc();

    const burnShares = new anchor.BN(5_000 * 1e6); // Burn 50% shares

    await program.methods
      .withdraw(burnShares)
      .accounts({
        user: alice.publicKey,
        vault: vaultPda,
        baseVault: baseVaultPda,
        shareMint: shareMintPda,
        userBaseToken: aliceBaseAccount,
        userShareToken: aliceShareAccount,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
      })
      .signers([alice])
      .rpc();

    const vault = await program.account.vaultState.fetch(vaultPda);
    expect(vault.totalManagedBase.toNumber()).to.equal(5_000 * 1e6);
  });
});