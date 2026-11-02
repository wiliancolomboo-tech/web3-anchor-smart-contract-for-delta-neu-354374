import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { Keypair, PublicKey } from "@solana/web3.js";
import { createMint, getOrCreateAssociatedTokenAccount, mintTo } from "@solana/spl-token";
import { expect } from "chai";
import { AegisDeltaRouter } from "../target/types/aegis_delta_router";

describe("aegis-delta-router test suite", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.AegisDeltaRouter as Program<AegisDeltaRouter>;
  const admin = Keypair.generate();
  const keeper = Keypair.generate();
  const user = Keypair.generate();

  let collateralMint: PublicKey;
  let debtMint: PublicKey;
  let strategyVaultPda: PublicKey;
  let vaultAuthorityPda: PublicKey;

  before(async () => {
    // Fund test wallets
    for (const kp of [admin, keeper, user]) {
      const sig = await provider.connection.requestAirdrop(kp.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
      await provider.connection.confirmTransaction(sig);
    }

    // Initialize Mock Token Mints
    collateralMint = await createMint(provider.connection, admin, admin.publicKey, null, 9);
    debtMint = await createMint(provider.connection, admin, admin.publicKey, null, 6);

    // Derive PDAs
    [strategyVaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("aegis_vault"), collateralMint.toBuffer(), debtMint.toBuffer()],
      program.programId
    );

    [vaultAuthorityPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("aegis_authority")],
      program.programId
    );
  });

  it("1. Successfully initializes the strategy with deterministic bounds", async () => {
    const targetLeverageBps = new anchor.BN(10_000); // 1.0x Hedge
    const maxSlippageBps = new anchor.BN(50);         // 0.50%
    const driftThresholdBps = new anchor.BN(150);     // 1.50%

    // Dummy protocols accounts
    const dummyKaminoVault = Keypair.generate().publicKey;
    const dummyMarginfiAccount = Keypair.generate().publicKey;

    await program.methods
      .initializeStrategy(targetLeverageBps, maxSlippageBps, driftThresholdBps)
      .accounts({
        strategyVault: strategyVaultPda,
        vaultAuthority: vaultAuthorityPda,
        collateralMint: collateralMint,
        debtMint: debtMint,
        kaminoVault: dummyKaminoVault,
        marginfiAccount: dummyMarginfiAccount,
        admin: admin.publicKey,
        keeper: keeper.publicKey,
        systemProgram: anchor.web3.SystemProgram.programId,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .signers([admin])
      .rpc();

    const vault = await program.account.strategyVault.fetch(strategyVaultPda);
    expect(vault.admin.toBase58()).to.equal(admin.publicKey.toBase58());
    expect(vault.keeper.toBase58()).to.equal(keeper.publicKey.toBase58());
    expect(vault.isPaused).to.be.false;
  });

  it("2. Rejects initialization if drift tolerance exceeds safety threshold (>300 BPS)", async () => {
    const invalidDriftBps = new anchor.BN(500); // 5%
    const dummyVault = Keypair.generate().publicKey;

    try {
      await program.methods
        .initializeStrategy(new anchor.BN(10_000), new anchor.BN(50), invalidDriftBps)
        .accounts({
          strategyVault: Keypair.generate().publicKey,
          vaultAuthority: vaultAuthorityPda,
          collateralMint: collateralMint,
          debtMint: debtMint,
          kaminoVault: dummyVault,
          marginfiAccount: dummyVault,
          admin: admin.publicKey,
          keeper: keeper.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
          tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
          rent: anchor.web3.SYSVAR_RENT_PUBKEY,
        })
        .signers([admin])
        .rpc();
      expect.fail("Should have failed on safety check");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("DriftThresholdTooHigh");
    }
  });

  it("3. Enforces Emergency Pause constraints", async () => {
    // Admin toggles pause
    await program.methods
      .setEmergencyPause(true)
      .accounts({
        strategyVault: strategyVaultPda,
        admin: admin.publicKey,
      })
      .signers([admin])
      .rpc();

    let vault = await program.account.strategyVault.fetch(strategyVaultPda);
    expect(vault.isPaused).to.be.true;

    // Non-admin attempt to unpause must fail
    try {
      await program.methods
        .setEmergencyPause(false)
        .accounts({
          strategyVault: strategyVaultPda,
          admin: user.publicKey,
        })
        .signers([user])
        .rpc();
      expect.fail("Unauthorized unpause succeeded");
    } catch (err: any) {
      expect(err.toString()).to.include("ConstraintHasOne");
    }

    // Re-enable operations
    await program.methods
      .setEmergencyPause(false)
      .accounts({
        strategyVault: strategyVaultPda,
        admin: admin.publicKey,
      })
      .signers([admin])
      .rpc();
  });
});