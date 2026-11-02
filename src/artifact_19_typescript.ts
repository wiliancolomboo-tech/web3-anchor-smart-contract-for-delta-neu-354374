import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { DeltaNeutralRouter } from "../target/types/delta_neutral_router";
import {
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { expect } from "chai";

describe("Delta-Neutral Yield Router Engine - Comprehensive Test Suite", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace
    .DeltaNeutralRouter as Program<DeltaNeutralRouter>;

  const authority = (provider.wallet as anchor.Wallet).payer;
  const keeper = Keypair.generate();
  const alice = Keypair.generate();

  let collateralMint: PublicKey;
  let borrowedMint: PublicKey;
  let kaminoCollateralMint: PublicKey;

  let vaultPda: PublicKey;
  let vaultBump: number;
  let strategyAuthorityPda: PublicKey;
  let strategyBump: number;
  let alicePositionPda: PublicKey;

  let aliceCollateralAta: PublicKey;
  let strategyCollateralAta: PublicKey;
  let strategyBorrowAta: PublicKey;
  let strategyKaminoCollateralAta: PublicKey;
  let keeperRewardAta: PublicKey;

  const mockKaminoProgram = Keypair.generate();
  const mockMarginfiProgram = Keypair.generate();
  const mockKaminoMarket = Keypair.generate();
  const mockKaminoReserve = Keypair.generate();
  const mockKaminoSupplyVault = Keypair.generate();
  const mockMarginfiGroup = Keypair.generate();
  const mockMarginfiBank = Keypair.generate();
  const mockMarginfiBankVault = Keypair.generate();
  const mockMarginfiAccount = Keypair.generate();

  const vaultId = new anchor.BN(42);
  const targetLtvBps = new anchor.BN(5000); // 50% LTV
  const driftThresholdBps = new anchor.BN(150); // 1.5% Drift

  before(async () => {
    // Air-drop funds to Alice and Keeper
    for (const recipient of [alice.publicKey, keeper.publicKey]) {
      const sig = await provider.connection.requestAirdrop(
        recipient,
        10 * anchor.web3.LAMPORTS_PER_SOL
      );
      await provider.connection.confirmTransaction(sig);
    }

    // Create Test SPL Mints
    collateralMint = await createMint(
      provider.connection,
      authority,
      authority.publicKey,
      null,
      6
    );
    borrowedMint = await createMint(
      provider.connection,
      authority,
      authority.publicKey,
      null,
      9
    );
    kaminoCollateralMint = await createMint(
      provider.connection,
      authority,
      authority.publicKey,
      null,
      6
    );

    // Derive PDAs
    [vaultPda, vaultBump] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), vaultId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    [strategyAuthorityPda, strategyBump] = PublicKey.findProgramAddressSync(
      [Buffer.from("strategy"), vaultPda.toBuffer()],
      program.programId
    );

    [alicePositionPda] = PublicKey.findProgramAddressSync(
      [
        Buffer.from("user_position"),
        vaultPda.toBuffer(),
        alice.publicKey.toBuffer(),
      ],
      program.programId
    );

    // Create Token Accounts
    aliceCollateralAta = await createAccount(
      provider.connection,
      alice,
      collateralMint,
      alice.publicKey
    );
    strategyCollateralAta = await createAccount(
      provider.connection,
      authority,
      collateralMint,
      strategyAuthorityPda
    );
    strategyBorrowAta = await createAccount(
      provider.connection,
      authority,
      borrowedMint,
      strategyAuthorityPda
    );
    strategyKaminoCollateralAta = await createAccount(
      provider.connection,
      authority,
      kaminoCollateralMint,
      strategyAuthorityPda
    );
    keeperRewardAta = await createAccount(
      provider.connection,
      keeper,
      collateralMint,
      keeper.publicKey
    );

    // Mint initial test balances (10,000 USDC to Alice)
    await mintTo(
      provider.connection,
      authority,
      collateralMint,
      aliceCollateralAta,
      authority,
      10_000 * 1e6
    );
  });

  it("QA-01: Initializes Global Vault with parameterized risk thresholds", async () => {
    const tx = await program.methods
      .initializeVault(vaultId, driftThresholdBps, targetLtvBps)
      .accounts({
        authority: authority.publicKey,
        vault: vaultPda,
        collateralMint: collateralMint,
        borrowedMint: borrowedMint,
        kaminoReserve: mockKaminoReserve.publicKey,
        marginfiBank: mockMarginfiBank.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
        rent: SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    expect(tx).to.be.a("string");

    const vaultAccount = await program.account.globalVault.fetch(vaultPda);
    expect(vaultAccount.vaultId.toString()).to.equal(vaultId.toString());
    expect(vaultAccount.driftThresholdBps.toNumber()).to.equal(150);
    expect(vaultAccount.targetLtvBps.toNumber()).to.equal(5000);
    expect(vaultAccount.isPaused).to.be.false;
  });

  it("QA-02: Rejects deposit when amount is zero", async () => {
    try {
      await program.methods
        .depositAndHedge(new anchor.BN(0), new anchor.BN(0))
        .accounts({
          user: alice.publicKey,
          vault: vaultPda,
          userPosition: alicePositionPda,
          strategyAuthority: strategyAuthorityPda,
          userCollateralAta: aliceCollateralAta,
          strategyCollateralAta: strategyCollateralAta,
          strategyBorrowAta: strategyBorrowAta,
          kaminoProgram: mockKaminoProgram.publicKey,
          kaminoLendingMarket: mockKaminoMarket.publicKey,
          kaminoReserve: mockKaminoReserve.publicKey,
          kaminoReserveLiquiditySupply: mockKaminoSupplyVault.publicKey,
          kaminoCollateralMint: kaminoCollateralMint,
          strategyKaminoDestinationCollateral: strategyKaminoCollateralAta,
          marginfiProgram: mockMarginfiProgram.publicKey,
          marginfiGroup: mockMarginfiGroup.publicKey,
          marginfiAccount: mockMarginfiAccount.publicKey,
          marginfiBank: mockMarginfiBank.publicKey,
          marginfiBankVault: mockMarginfiBankVault.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
          rent: SYSVAR_RENT_PUBKEY,
        })
        .signers([alice])
        .rpc();
      expect.fail("Expected zero deposit to fail");
    } catch (err: any) {
      expect(err.error?.errorCode?.code || err.toString()).to.include(
        "ZeroAmountInvalid"
      );
    }
  });

  it("QA-03: Rebalances delta when drift exceeds threshold (Keeper trigger)", async () => {
    // Spot value = $10,500, Hedge value = $10,000 -> Drift = 500 / 20500 = 2.43% (> 1.5%)
    const spotVal = new anchor.BN("10500000000000000000000"); // 10,500 WAD
    const hedgeVal = new anchor.BN("10000000000000000000000"); // 10,000 WAD
    const rebalanceAmt = new anchor.BN(500 * 1e6);

    const tx = await program.methods
      .rebalanceDrift(spotVal, hedgeVal, rebalanceAmt)
      .accounts({
        keeper: keeper.publicKey,
        vault: vaultPda,
        strategyAuthority: strategyAuthorityPda,
        keeperRewardAta: keeperRewardAta,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([keeper])
      .rpc();

    expect(tx).to.be.a("string");
    const vaultAccount = await program.account.globalVault.fetch(vaultPda);
    expect(vaultAccount.totalBorrowedHedge.toString()).to.equal(
      rebalanceAmt.toString()
    );
  });

  it("QA-04: Rejects rebalance when drift is within normal tolerance bounds", async () => {
    // Spot value = $10,050, Hedge value = $10,000 -> Drift = 50 / 20050 = 0.24% (< 1.5%)
    const spotVal = new anchor.BN("10050000000000000000000");
    const hedgeVal = new anchor.BN("10000000000000000000000");
    const rebalanceAmt = new anchor.BN(50 * 1e6);

    try {
      await program.methods
        .rebalanceDrift(spotVal, hedgeVal, rebalanceAmt)
        .accounts({
          keeper: keeper.publicKey,
          vault: vaultPda,
          strategyAuthority: strategyAuthorityPda,
          keeperRewardAta: keeperRewardAta,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([keeper])
        .rpc();
      expect.fail("Rebalance within threshold should fail");
    } catch (err: any) {
      expect(err.error?.errorCode?.code || err.toString()).to.include(
        "DriftWithinThreshold"
      );
    }
  });

  it("QA-05: Emergency Circuit Breaker prevents operations when halted", async () => {
    // Pause vault
    await program.methods
      .emergencyPause(true)
      .accounts({
        authority: authority.publicKey,
        vault: vaultPda,
      })
      .rpc();

    let vaultAccount = await program.account.globalVault.fetch(vaultPda);
    expect(vaultAccount.isPaused).to.be.true;

    // Attempted rebalance must fail
    const spotVal = new anchor.BN("12000000000000000000000");
    const hedgeVal = new anchor.BN("10000000000000000000000");
    try {
      await program.methods
        .rebalanceDrift(spotVal, hedgeVal, new anchor.BN(1000))
        .accounts({
          keeper: keeper.publicKey,
          vault: vaultPda,
          strategyAuthority: strategyAuthorityPda,
          keeperRewardAta: keeperRewardAta,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([keeper])
        .rpc();
      expect.fail("Paused vault must block rebalance");
    } catch (err: any) {
      expect(err.error?.errorCode?.code || err.toString()).to.include(
        "VaultPaused"
      );
    }

    // Unpause vault
    await program.methods
      .emergencyPause(false)
      .accounts({
        authority: authority.publicKey,
        vault: vaultPda,
      })
      .rpc();

    vaultAccount = await program.account.globalVault.fetch(vaultPda);
    expect(vaultAccount.isPaused).to.be.false;
  });
});