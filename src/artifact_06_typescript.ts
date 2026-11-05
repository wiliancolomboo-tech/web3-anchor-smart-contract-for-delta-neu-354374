import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { Keypair, PublicKey, SystemProgram } from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { expect } from "chai";
import { DeltaNeutralRouter } from "../target/types/delta_neutral_router";

describe("Delta-Neutral Yield Router Test Matrix", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.DeltaNeutralRouter as Program<DeltaNeutralRouter>;

  let admin: Keypair;
  let depositor: Keypair;
  let depositMintUsdc: PublicKey;
  let hedgedMintSol: PublicKey;
  let vaultStatePda: PublicKey;
  let vaultAuthorityPda: PublicKey;
  let userUsdcAta: PublicKey;
  let vaultUsdcAta: PublicKey;
  let vaultHedgedAta: PublicKey;
  let kaminoCollateralAta: PublicKey;
  let mockPythPriceFeed: Keypair;
  let mockKaminoReserve: Keypair;
  let mockMarginfiBank: Keypair;
  let mockMarginfiAccount: Keypair;

  before(async () => {
    admin = Keypair.generate();
    depositor = Keypair.generate();
    mockPythPriceFeed = Keypair.generate();
    mockKaminoReserve = Keypair.generate();
    mockMarginfiBank = Keypair.generate();
    mockMarginfiAccount = Keypair.generate();

    // Airdrop SOL
    for (const kp of [admin, depositor]) {
      const sig = await provider.connection.requestAirdrop(kp.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
      await provider.connection.confirmTransaction(sig);
    }

    // Create Mints
    depositMintUsdc = await createMint(provider.connection, admin, admin.publicKey, null, 6);
    hedgedMintSol = await createMint(provider.connection, admin, admin.publicKey, null, 9);

    // Derive PDAs
    [vaultStatePda] = PublicKey.findProgramAddressSync(
      [Buffer.from("delta_vault"), depositMintUsdc.toBuffer(), hedgedMintSol.toBuffer()],
      program.programId
    );

    [vaultAuthorityPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault_authority"), vaultStatePda.toBuffer()],
      program.programId
    );

    // Initialize Token Accounts
    userUsdcAta = await createAccount(provider.connection, depositor, depositMintUsdc, depositor.publicKey);
    vaultUsdcAta = await createAccount(provider.connection, admin, depositMintUsdc, vaultAuthorityPda);
    vaultHedgedAta = await createAccount(provider.connection, admin, hedgedMintSol, vaultAuthorityPda);
    kaminoCollateralAta = await createAccount(provider.connection, admin, depositMintUsdc, vaultAuthorityPda);

    // Mint 10,000 USDC to user
    await mintTo(provider.connection, admin, depositMintUsdc, userUsdcAta, admin, 10_000 * 1e6);
  });

  it("1. Successfully initializes the vault state PDA with verified parameters", async () => {
    const targetLeverageBps = new anchor.BN(10_000); // 1.0x (fully hedged delta neutral)
    const rebalanceThresholdBps = new anchor.BN(100); // 1.00%

    await program.methods
      .initializeVault(targetLeverageBps, rebalanceThresholdBps)
      .accounts({
        admin: admin.publicKey,
        vaultState: vaultStatePda,
        vaultAuthority: vaultAuthorityPda,
        depositMint: depositMintUsdc,
        hedgedMint: hedgedMintSol,
        kaminoReserve: mockKaminoReserve.publicKey,
        marginfiBank: mockMarginfiBank.publicKey,
        pythPriceFeed: mockPythPriceFeed.publicKey,
        systemProgram: SystemProgram.programId,
        tokenProgram: TOKEN_PROGRAM_ID,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .signers([admin])
      .rpc();

    const vaultAccount = await program.account.vaultState.fetch(vaultStatePda);
    expect(vaultAccount.admin.toBase58()).to.equal(admin.publicKey.toBase58());
    expect(vaultAccount.targetLeverageBps.toNumber()).to.equal(10000);
    expect(vaultAccount.rebalanceThresholdBps.toNumber()).to.equal(100);
    expect(vaultAccount.totalUsdcDeposited.toNumber()).to.equal(0);
    expect(vaultAccount.isFrozen).to.be.false;
  });

  it("2. Fails to initialize with excessive rebalance thresholds (>10%)", async () => {
    const invalidThreshold = new anchor.BN(2000); // 20%
    const dummyDeposit = await createMint(provider.connection, admin, admin.publicKey, null, 6);
    const [dummyVault] = PublicKey.findProgramAddressSync(
      [Buffer.from("delta_vault"), dummyDeposit.toBuffer(), hedgedMintSol.toBuffer()],
      program.programId
    );
    const [dummyAuth] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault_authority"), dummyVault.toBuffer()],
      program.programId
    );

    try {
      await program.methods
        .initializeVault(new anchor.BN(10_000), invalidThreshold)
        .accounts({
          admin: admin.publicKey,
          vaultState: dummyVault,
          vaultAuthority: dummyAuth,
          depositMint: dummyDeposit,
          hedgedMint: hedgedMintSol,
          kaminoReserve: mockKaminoReserve.publicKey,
          marginfiBank: mockMarginfiBank.publicKey,
          pythPriceFeed: mockPythPriceFeed.publicKey,
          systemProgram: SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
          rent: anchor.web3.SYSVAR_RENT_PUBKEY,
        })
        .signers([admin])
        .rpc();
      expect.fail("Should have thrown InvalidThreshold");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("InvalidThreshold");
    }
  });

  it("3. Enforces Emergency Pause constraints", async () => {
    // Admin freezes vault
    await program.methods
      .setVaultFrozen(true)
      .accounts({
        vaultState: vaultStatePda,
        admin: admin.publicKey,
      })
      .signers([admin])
      .rpc();

    let vaultState = await program.account.vaultState.fetch(vaultStatePda);
    expect(vaultState.isFrozen).to.be.true;

    // Unauthorized non-admin cannot unfreeze
    try {
      await program.methods
        .setVaultFrozen(false)
        .accounts({
          vaultState: vaultStatePda,
          admin: depositor.publicKey,
        })
        .signers([depositor])
        .rpc();
      expect.fail("Unauthorized account was able to toggle vault pause");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("UnauthorizedAdmin");
    }

    // Restore unfreeze state for remaining test suite
    await program.methods
      .setVaultFrozen(false)
      .accounts({
        vaultState: vaultStatePda,
        admin: admin.publicKey,
      })
      .signers([admin])
      .rpc();

    vaultState = await program.account.vaultState.fetch(vaultStatePda);
    expect(vaultState.isFrozen).to.be.false;
  });

  it("4. Prevents zero deposit attempts", async () => {
    try {
      await program.methods
        .depositAndHedge(new anchor.BN(0))
        .accounts({
          depositor: depositor.publicKey,
          vaultState: vaultStatePda,
          vaultAuthority: vaultAuthorityPda,
          userUsdcAta: userUsdcAta,
          vaultUsdcAta: vaultUsdcAta,
          vaultHedgedAta: vaultHedgedAta,
          kaminoCollateralAta: kaminoCollateralAta,
          pythPriceFeed: mockPythPriceFeed.publicKey,
          kaminoProgram: program.programId, // Mock self-call target for isolation
          kaminoReserve: mockKaminoReserve.publicKey,
          marginfiProgram: program.programId,
          marginfiBank: mockMarginfiBank.publicKey,
          marginfiAccount: mockMarginfiAccount.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([depositor])
        .rpc();
      expect.fail("Should have failed on zero deposit");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("ZeroDepositAmount");
    }
  });
});