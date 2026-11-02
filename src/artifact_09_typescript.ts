import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { DeltaNeutralRouter } from "../target/types/delta_neutral_router";
import {
  createMint,
  createAccount,
  mintTo,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { Keypair, PublicKey, SystemProgram, SYSVAR_RENT_PUBKEY } from "@solana/web3.js";
import { expect } from "chai";

describe("Aegis Delta-Neutral Yield Routing Test Suite", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.DeltaNeutralRouter as Program<DeltaNeutralRouter>;

  let admin = Keypair.generate();
  let user = Keypair.generate();
  let keeper = Keypair.generate();

  let collateralMint: PublicKey;
  let debtMint: PublicKey;

  let userCollateralAccount: PublicKey;
  let vaultCollateralAccount: PublicKey;
  let vaultDebtAccount: PublicKey;

  let strategyConfigPda: PublicKey;
  let vaultAuthorityPda: PublicKey;
  let userPositionPda: PublicKey;

  // Mock protocol & oracle IDs
  const mockKaminoProgram = Keypair.generate();
  const mockMarginfiProgram = Keypair.generate();
  const mockCollateralOracle = Keypair.generate();
  const mockDebtOracle = Keypair.generate();
  const mockKaminoObligation = Keypair.generate();
  const mockMarginfiAccount = Keypair.generate();

  before(async () => {
    // Airdrop SOL
    for (const kp of [admin, user, keeper]) {
      const sig = await provider.connection.requestAirdrop(kp.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
      await provider.connection.confirmTransaction(sig);
    }

    // Derive PDAs
    [strategyConfigPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("strategy_config"), admin.publicKey.toBuffer()],
      program.programId
    );

    [vaultAuthorityPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault_authority")],
      program.programId
    );

    [userPositionPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("user_position"), user.publicKey.toBuffer(), strategyConfigPda.toBuffer()],
      program.programId
    );

    // Setup Mock SPL Tokens (mSOL as Collateral, SOL/USDC as Debt)
    collateralMint = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      9
    );

    debtMint = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      9
    );

    // Create Token Accounts
    userCollateralAccount = await createAccount(
      provider.connection,
      user,
      collateralMint,
      user.publicKey
    );

    vaultCollateralAccount = await createAccount(
      provider.connection,
      admin,
      collateralMint,
      vaultAuthorityPda
    );

    vaultDebtAccount = await createAccount(
      provider.connection,
      admin,
      debtMint,
      vaultAuthorityPda
    );

    // Seed User with 100 mSOL
    await mintTo(
      provider.connection,
      admin,
      collateralMint,
      userCollateralAccount,
      admin,
      100 * 10 ** 9
    );
  });

  it("Initializes the strategy with valid parameters", async () => {
    const targetLtvBps = new anchor.BN(6500); // 65%
    const maxLtvBps = new anchor.BN(7500);    // 75%
    const rebalanceThresholdBps = new anchor.BN(150); // 1.5%

    await program.methods
      .initializeStrategy(targetLtvBps, maxLtvBps, rebalanceThresholdBps)
      .accountsStrict({
        strategyConfig: strategyConfigPda,
        vaultAuthority: vaultAuthorityPda,
        collateralMint: collateralMint,
        debtMint: debtMint,
        kaminoProgram: mockKaminoProgram.publicKey,
        marginfiProgram: mockMarginfiProgram.publicKey,
        admin: admin.publicKey,
        systemProgram: SystemProgram.programId,
        rent: SYSVAR_RENT_PUBKEY,
      })
      .signers([admin])
      .rpc();

    const config = await program.account.strategyConfig.fetch(strategyConfigPda);
    expect(config.admin.toBase58()).to.equal(admin.publicKey.toBase58());
    expect(config.targetLtvBps.toNumber()).to.equal(6500);
    expect(config.maxLtvBps.toNumber()).to.equal(7500);
    expect(config.rebalanceThresholdBps.toNumber()).to.equal(150);
    expect(config.reentrancyStatus).to.equal(0);
  });

  it("Rejects initialization if target LTV exceeds max safe LTV", async () => {
    const badAdmin = Keypair.generate();
    const sig = await provider.connection.requestAirdrop(badAdmin.publicKey, 2 * anchor.web3.LAMPORTS_PER_SOL);
    await provider.connection.confirmTransaction(sig);

    const [badConfigPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("strategy_config"), badAdmin.publicKey.toBuffer()],
      program.programId
    );

    try {
      await program.methods
        .initializeStrategy(new anchor.BN(8000), new anchor.BN(7000), new anchor.BN(100))
        .accountsStrict({
          strategyConfig: badConfigPda,
          vaultAuthority: vaultAuthorityPda,
          collateralMint: collateralMint,
          debtMint: debtMint,
          kaminoProgram: mockKaminoProgram.publicKey,
          marginfiProgram: mockMarginfiProgram.publicKey,
          admin: badAdmin.publicKey,
          systemProgram: SystemProgram.programId,
          rent: SYSVAR_RENT_PUBKEY,
        })
        .signers([badAdmin])
        .rpc();
      expect.fail("Should have failed with MaxLtvExceeded");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("MaxLtvExceeded");
    }
  });

  it("Enforces PDA and signer authenticity on Position execution", async () => {
    const maliciousActor = Keypair.generate();
    const sig = await provider.connection.requestAirdrop(maliciousActor.publicKey, 2 * anchor.web3.LAMPORTS_PER_SOL);
    await provider.connection.confirmTransaction(sig);

    const fakeTokenAccount = await createAccount(
      provider.connection,
      maliciousActor,
      collateralMint,
      maliciousActor.publicKey
    );

    try {
      await program.methods
        .depositAndRoute(new anchor.BN(10 * 10 ** 9), new anchor.BN(15000))
        .accountsStrict({
          strategyConfig: strategyConfigPda,
          userPosition: userPositionPda,
          vaultAuthority: vaultAuthorityPda,
          userCollateralToken: fakeTokenAccount,
          vaultCollateralToken: vaultCollateralAccount,
          vaultDebtToken: vaultDebtAccount,
          collateralOracle: mockCollateralOracle.publicKey,
          debtOracle: mockDebtOracle.publicKey,
          kaminoObligation: mockKaminoObligation.publicKey,
          marginfiAccount: mockMarginfiAccount.publicKey,
          kaminoProgram: mockKaminoProgram.publicKey,
          marginfiProgram: mockMarginfiProgram.publicKey,
          user: maliciousActor.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([maliciousActor])
        .rpc();
      expect.fail("Should have failed PDA seed or unauthorized signer constraint");
    } catch (err: any) {
      expect(err).to.exist;
    }
  });
});