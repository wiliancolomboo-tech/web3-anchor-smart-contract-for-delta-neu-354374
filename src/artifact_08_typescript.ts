import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { AuraDeltaRouter } from "../target/types/aura_delta_router";
import {
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { Keypair, PublicKey, SystemProgram } from "@solana/web3.js";
import { expect } from "chai";

describe("Aura-Delta Yield Router Engine Tests", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.AuraDeltaRouter as Program<AuraDeltaRouter>;

  let usdcMint: PublicKey;
  let solMint: PublicKey;
  let vaultPda: PublicKey;
  let vaultBump: number;
  let vaultUsdcAta: PublicKey;
  let admin = Keypair.generate();
  let keeper = Keypair.generate();
  let user = Keypair.generate();
  let userUsdcAccount: PublicKey;
  let userPositionPda: PublicKey;

  // Mock protocol accounts
  const dummyMarginfiProgram = Keypair.generate();
  const dummyMarginfiAccount = Keypair.generate();
  const dummyKaminoProgram = Keypair.generate();
  const dummyKaminoStrategy = Keypair.generate();

  before(async () => {
    // Airdrop SOL to test actors
    for (const actor of [admin, keeper, user]) {
      const sig = await provider.connection.requestAirdrop(actor.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
      await provider.connection.confirmTransaction(sig);
    }

    // Create Test USDC & SOL Mints
    usdcMint = await createMint(provider.connection, admin, admin.publicKey, null, 6);
    solMint = await createMint(provider.connection, admin, admin.publicKey, null, 9);

    [vaultPda, vaultBump] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault_state"), usdcMint.toBuffer()],
      program.programId
    );

    [vaultUsdcAta] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault_usdc_ata"), vaultPda.toBuffer()],
      program.programId
    );

    [userPositionPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("user_position"), vaultPda.toBuffer(), user.publicKey.toBuffer()],
      program.programId
    );

    // Setup user token account and mint initial USDC
    userUsdcAccount = await createAccount(provider.connection, user, usdcMint, user.publicKey);
    await mintTo(provider.connection, admin, usdcMint, userUsdcAccount, admin.publicKey, 100_000_000_000); // 100k USDC
  });

  it("1. Initializes Global Vault PDA with strict risk parameters", async () => {
    const deltaToleranceBps = new anchor.BN(20); // 20 BPS (0.2%)
    const targetHedgeRatioBps = new anchor.BN(5000); // 50%

    await program.methods
      .initializeVault(deltaToleranceBps, targetHedgeRatioBps, vaultBump)
      .accounts({
        vault: vaultPda,
        usdcMint: usdcMint,
        solMint: solMint,
        vaultUsdcAccount: vaultUsdcAta,
        vaultSigner: vaultPda,
        admin: admin.publicKey,
        keeper: keeper.publicKey,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .signers([admin])
      .rpc();

    const vaultState = await program.account.vaultState.fetch(vaultPda);
    expect(vaultState.admin.toBase58()).to.equal(admin.publicKey.toBase58());
    expect(vaultState.keeper.toBase58()).to.equal(keeper.publicKey.toBase58());
    expect(vaultState.deltaToleranceBps.toNumber()).to.equal(20);
    expect(vaultState.isPaused).to.be.false;
  });

  it("2. User deposits USDC and receives proportional 1:1 initial LP shares", async () => {
    const depositAmount = new anchor.BN(10_000_000_000); // 10,000 USDC
    const minSharesOut = new anchor.BN(10_000_000_000);

    await program.methods
      .deposit(depositAmount, minSharesOut)
      .accounts({
        vault: vaultPda,
        userPosition: userPositionPda,
        userUsdcSource: userUsdcAccount,
        vaultUsdcAccount: vaultUsdcAta,
        user: user.publicKey,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([user])
      .rpc();

    const userPos = await program.account.userPosition.fetch(userPositionPda);
    const vaultState = await program.account.vaultState.fetch(vaultPda);
    const vaultAtaInfo = await getAccount(provider.connection, vaultUsdcAta);

    expect(userPos.shares.toString()).to.equal(depositAmount.toString());
    expect(vaultState.totalVaultShares.toString()).to.equal(depositAmount.toString());
    expect(vaultState.totalUsdcDeposited.toString()).to.equal(depositAmount.toString());
    expect(Number(vaultAtaInfo.amount)).to.equal(depositAmount.toNumber());
  });

  it("3. Enforces Keeper Authorization during Rebalance", async () => {
    const unauthorizedActor = Keypair.generate();
    const sig = await provider.connection.requestAirdrop(unauthorizedActor.publicKey, 10 ** 9);
    await provider.connection.confirmTransaction(sig);

    try {
      await program.methods
        .rebalance(
          new anchor.BN(150_000_000),
          new anchor.BN(1_000_000_000),
          new anchor.BN(1_000_000_000)
        )
        .accounts({
          vault: vaultPda,
          vaultSigner: vaultPda,
          keeper: unauthorizedActor.publicKey,
          marginfiProgram: dummyMarginfiProgram.publicKey,
          marginfiAccount: dummyMarginfiAccount.publicKey,
          kaminoProgram: dummyKaminoProgram.publicKey,
          kaminoStrategy: dummyKaminoStrategy.publicKey,
        })
        .signers([unauthorizedActor])
        .rpc();
      expect.fail("Unauthorized keeper must be rejected");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("UnauthorizedKeeper");
    }
  });

  it("4. Rejects Rebalance when Net Delta exceeds delta_tolerance_bps", async () => {
    // Spot: 1.00 SOL (1e9 lamports), Hedge Borrow: 0.90 SOL (0.9e9 lamports)
    // Drift = 10% (1,000 bps) -> Exceeds 20 bps bound
    const spotSol = new anchor.BN(1_000_000_000);
    const borrowSol = new anchor.BN(900_000_000);

    try {
      await program.methods
        .rebalance(new anchor.BN(150_000_000), spotSol, borrowSol)
        .accounts({
          vault: vaultPda,
          vaultSigner: vaultPda,
          keeper: keeper.publicKey,
          marginfiProgram: dummyMarginfiProgram.publicKey,
          marginfiAccount: dummyMarginfiAccount.publicKey,
          kaminoProgram: dummyKaminoProgram.publicKey,
          kaminoStrategy: dummyKaminoStrategy.publicKey,
        })
        .signers([keeper])
        .rpc();
      expect.fail("Drift exceeding delta tolerance must fail");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("DeltaExceedsBound");
    }
  });

  it("5. Emergency Admin Control: Pause prevents new deposits", async () => {
    // Admin toggles pause
    await program.methods
      .setEmergencyPause(true)
      .accounts({
        vault: vaultPda,
        admin: admin.publicKey,
      })
      .signers([admin])
      .rpc();

    let vaultState = await program.account.vaultState.fetch(vaultPda);
    expect(vaultState.isPaused).to.be.true;

    // User attempts deposit while paused
    try {
      await program.methods
        .deposit(new anchor.BN(1_000_000), new anchor.BN(1_000_000))
        .accounts({
          vault: vaultPda,
          userPosition: userPositionPda,
          userUsdcSource: userUsdcAccount,
          vaultUsdcAccount: vaultUsdcAta,
          user: user.publicKey,
          tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([user])
        .rpc();
      expect.fail("Deposits must fail when vault is paused");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("VaultPaused");
    }

    // Unpause for subsequent operations
    await program.methods
      .setEmergencyPause(false)
      .accounts({
        vault: vaultPda,
        admin: admin.publicKey,
      })
      .signers([admin])
      .rpc();
  });

  it("6. User successfully burns shares and withdraws USDC capital", async () => {
    const sharesToBurn = new anchor.BN(5_000_000_000); // 50% of user balance
    const minUsdcOut = new anchor.BN(5_000_000_000);

    const initialUserBal = (await getAccount(provider.connection, userUsdcAccount)).amount;

    await program.methods
      .withdraw(sharesToBurn, minUsdcOut)
      .accounts({
        vault: vaultPda,
        userPosition: userPositionPda,
        vaultUsdcAccount: vaultUsdcAta,
        userUsdcDestination: userUsdcAccount,
        vaultSigner: vaultPda,
        user: user.publicKey,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
      })
      .signers([user])
      .rpc();

    const postUserBal = (await getAccount(provider.connection, userUsdcAccount)).amount;
    const userPos = await program.account.userPosition.fetch(userPositionPda);

    expect(Number(postUserBal) - Number(initialUserBal)).to.equal(sharesToBurn.toNumber());
    expect(userPos.shares.toNumber()).to.equal(5_000_000_000);
  });
});