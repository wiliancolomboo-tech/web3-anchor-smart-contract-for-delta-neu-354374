// tests/aegis-delta-neutral.ts

import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { AegisDeltaNeutral } from "../target/types/aegis_delta_neutral";
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

describe("Aegis Delta-Neutral Vault Program Suite", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.AegisDeltaNeutral as Program<AegisDeltaNeutral>;

  let underlyingMint: PublicKey;
  let lpMintKeypair: Keypair;
  let vaultPda: PublicKey;
  let vaultBump: number;
  let vaultAuthorityPda: PublicKey;
  let authorityBump: number;
  let keeper: Keypair;
  let user: Keypair;
  let userTokenAccount: PublicKey;
  let userLpAccount: PublicKey;
  let vaultTokenAccount: PublicKey;

  // Mock Kamino & Marginfi PDAs
  const mockKaminoObligation = Keypair.generate().publicKey;
  const mockMarginfiAccount = Keypair.generate().publicKey;

  before(async () => {
    keeper = Keypair.generate();
    user = Keypair.generate();
    lpMintKeypair = Keypair.generate();

    // Airdrop SOL to test actors
    await provider.connection.confirmTransaction(
      await provider.connection.requestAirdrop(provider.wallet.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL)
    );
    await provider.connection.confirmTransaction(
      await provider.connection.requestAirdrop(user.publicKey, 5 * anchor.web3.LAMPORTS_PER_SOL)
    );
    await provider.connection.confirmTransaction(
      await provider.connection.requestAirdrop(keeper.publicKey, 5 * anchor.web3.LAMPORTS_PER_SOL)
    );

    // Create Base Collateral Token (USDC/SOL equivalent)
    underlyingMint = await createMint(
      provider.connection,
      (provider.wallet as any).payer,
      provider.wallet.publicKey,
      null,
      6
    );

    // Derive Program PDAs
    [vaultPda, vaultBump] = PublicKey.findProgramAddressSync(
      [Buffer.from("aegis_vault"), underlyingMint.toBuffer()],
      program.programId
    );

    [vaultAuthorityPda, authorityBump] = PublicKey.findProgramAddressSync(
      [Buffer.from("aegis_authority"), vaultPda.toBuffer()],
      program.programId
    );

    // Create Associated Token Accounts
    vaultTokenAccount = await createAccount(
      provider.connection,
      (provider.wallet as any).payer,
      underlyingMint,
      vaultAuthorityPda
    );

    userTokenAccount = await createAccount(
      provider.connection,
      (provider.wallet as any).payer,
      underlyingMint,
      user.publicKey
    );

    userLpAccount = await createAccount(
      provider.connection,
      (provider.wallet as any).payer,
      lpMintKeypair.publicKey,
      user.publicKey
    );

    // Mint underlying funds to user
    await mintTo(
      provider.connection,
      (provider.wallet as any).payer,
      underlyingMint,
      userTokenAccount,
      provider.wallet.publicKey,
      1_000_000_000 // 1,000 units
    );
  });

  it("1. Successfully initializes the delta-neutral vault with strict parameters", async () => {
    const targetLeverageBps = new anchor.BN(10_000); // 1.0x
    const rebalanceThresholdBps = new anchor.BN(50); // 0.50%

    const tx = await program.methods
      .initializeVault(targetLeverageBps, rebalanceThresholdBps)
      .accounts({
        vault: vaultPda,
        vaultAuthority: vaultAuthorityPda,
        underlyingMint: underlyingMint,
        lpMint: lpMintKeypair.publicKey,
        kaminoObligation: mockKaminoObligation,
        marginfiAccount: mockMarginfiAccount,
        keeper: keeper.publicKey,
        authority: provider.wallet.publicKey,
        systemProgram: SystemProgram.programId,
        tokenProgram: TOKEN_PROGRAM_ID,
        rent: SYSVAR_RENT_PUBKEY,
      })
      .signers([lpMintKeypair])
      .rpc();

    expect(tx).to.be.a("string");

    const vaultAccount = await program.account.vaultState.fetch(vaultPda);
    expect(vaultAccount.authority.toBase58()).to.equal(provider.wallet.publicKey.toBase58());
    expect(vaultAccount.keeper.toBase58()).to.equal(keeper.publicKey.toBase58());
    expect(vaultAccount.targetLeverageBps.toNumber()).to.equal(10_000);
    expect(vaultAccount.isPaused).to.be.false;
  });

  it("2. Permits LP deposit and mints 1:1 proportional initial LP shares", async () => {
    const depositAmount = new anchor.BN(500_000_000); // 500 units

    await program.methods
      .deposit(depositAmount)
      .accounts({
        vault: vaultPda,
        vaultAuthority: vaultAuthorityPda,
        vaultTokenAccount: vaultTokenAccount,
        userTokenAccount: userTokenAccount,
        lpMint: lpMintKeypair.publicKey,
        userLpAccount: userLpAccount,
        user: user.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([user])
      .rpc();

    const vaultAccount = await program.account.vaultState.fetch(vaultPda);
    expect(vaultAccount.totalDeposited.toNumber()).to.equal(500_000_000);

    const userLpToken = await getAccount(provider.connection, userLpAccount);
    expect(Number(userLpToken.amount)).to.equal(500_000_000);
  });

  it("3. Enforces authorization: Rejects unauthorized caller trying to trigger delta routing", async () => {
    const imposter = Keypair.generate();
    await provider.connection.confirmTransaction(
      await provider.connection.requestAirdrop(imposter.publicKey, 1 * anchor.web3.LAMPORTS_PER_SOL)
    );

    try {
      await program.methods
        .executeDeltaRoute(new anchor.BN(1000), new anchor.BN(1000), new anchor.BN(20_000_000))
        .accounts({
          vault: vaultPda,
          vaultAuthority: vaultAuthorityPda,
          vaultTokenAccount: vaultTokenAccount,
          vaultHedgeAccount: userTokenAccount,
          kaminoProgram: SystemProgram.programId,
          kaminoReserve: Keypair.generate().publicKey,
          kaminoObligation: mockKaminoObligation,
          marginfiProgram: SystemProgram.programId,
          marginfiGroup: Keypair.generate().publicKey,
          marginfiBank: Keypair.generate().publicKey,
          marginfiAccount: mockMarginfiAccount,
          signer: imposter.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([imposter])
        .rpc();
      expect.fail("Unauthorized caller should have been rejected");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("UnauthorizedCaller");
    }
  });

  it("4. Triggers emergency circuit breaker and blocks subsequent user deposits", async () => {
    await program.methods
      .emergencyPauseAndUnwind()
      .accounts({
        vault: vaultPda,
        authority: provider.wallet.publicKey,
      })
      .rpc();

    const vaultAccount = await program.account.vaultState.fetch(vaultPda);
    expect(vaultAccount.isPaused).to.be.true;

    // Verify Deposit reverts when paused
    try {
      await program.methods
        .deposit(new anchor.BN(10_000_000))
        .accounts({
          vault: vaultPda,
          vaultAuthority: vaultAuthorityPda,
          vaultTokenAccount: vaultTokenAccount,
          userTokenAccount: userTokenAccount,
          lpMint: lpMintKeypair.publicKey,
          userLpAccount: userLpAccount,
          user: user.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([user])
        .rpc();
      expect.fail("Deposit should have been rejected while vault is paused");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("VaultIsPaused");
    }
  });
});