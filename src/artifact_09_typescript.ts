import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { DeltaNeutralRouter } from "../target/types/delta_neutral_router";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { expect } from "chai";
import { PublicKey, Keypair, SystemProgram } from "@solana/web3.js";

describe("delta-neutral-router integration test matrix", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.DeltaNeutralRouter as Program<DeltaNeutralRouter>;

  let admin: Keypair;
  let user: Keypair;
  let assetMint: PublicKey;
  let userTokenAccount: PublicKey;
  let userShareAccount: PublicKey;
  let vaultPda: PublicKey;
  let vaultBump: number;
  let vaultAuthorityPda: PublicKey;
  let authBump: number;
  let shareMintPda: PublicKey;
  let vaultAssetAccount: PublicKey;

  const DECIMALS = 6;
  const INITIAL_USER_BALANCE = 1_000_000_000; // 1,000 USDC

  before(async () => {
    admin = Keypair.generate();
    user = Keypair.generate();

    // Fund test wallets
    const airdropAdmin = await provider.connection.requestAirdrop(admin.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
    const airdropUser = await provider.connection.requestAirdrop(user.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
    await provider.connection.confirmTransaction(airdropAdmin);
    await provider.connection.confirmTransaction(airdropUser);

    // Create Base Asset Mint (USDC equivalent)
    assetMint = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      DECIMALS
    );

    [vaultPda, vaultBump] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), assetMint.toBuffer()],
      program.programId
    );

    [vaultAuthorityPda, authBump] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), assetMint.toBuffer(), Buffer.from("authority")],
      program.programId
    );

    [shareMintPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), assetMint.toBuffer(), Buffer.from("shares")],
      program.programId
    );

    vaultAssetAccount = anchor.utils.token.associatedAddress({
      mint: assetMint,
      owner: vaultAuthorityPda,
    });

    // Create User Accounts
    userTokenAccount = await createAccount(
      provider.connection,
      user,
      assetMint,
      user.publicKey
    );

    userShareAccount = await createAccount(
      provider.connection,
      user,
      shareMintPda,
      user.publicKey
    );

    // Seed User Balance
    await mintTo(
      provider.connection,
      admin,
      assetMint,
      userTokenAccount,
      admin,
      INITIAL_USER_BALANCE
    );
  });

  it("1. Successfully initializes the delta-neutral routing vault", async () => {
    const rebalanceThresholdBps = 50; // 0.5%
    const targetLeverageBps = 20_000; // 2.0x

    const tx = await program.methods
      .initializeVault(rebalanceThresholdBps, targetLeverageBps)
      .accounts({
        admin: admin.publicKey,
        assetMint: assetMint,
        vault: vaultPda,
        vaultAuthority: vaultAuthorityPda,
        shareMint: shareMintPda,
        vaultAssetAccount: vaultAssetAccount,
        systemProgram: SystemProgram.programId,
        tokenProgram: TOKEN_PROGRAM_ID,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .signers([admin])
      .rpc();

    expect(tx).to.be.a("string");

    const vaultAccount = await program.account.vaultState.fetch(vaultPda);
    expect(vaultAccount.admin.toBase58()).to.equal(admin.publicKey.toBase58());
    expect(vaultAccount.rebalanceThresholdBps).to.equal(rebalanceThresholdBps);
    expect(vaultAccount.targetLeverageBps).to.equal(targetLeverageBps);
    expect(vaultAccount.totalUnderlyingDeposited.toNumber()).to.equal(0);
    expect(vaultAccount.isPaused).to.be.false;
  });

  it("2. Rejects initialization with excessive leverage (> 3x)", async () => {
    const excessiveLeverageBps = 35_000; // 3.5x (Hard cap is 3.0x)
    const fakeMint = Keypair.generate();

    try {
      const [fakeVault] = PublicKey.findProgramAddressSync(
        [Buffer.from("vault"), fakeMint.publicKey.toBuffer()],
        program.programId
      );
      const [fakeAuthority] = PublicKey.findProgramAddressSync(
        [Buffer.from("vault"), fakeMint.publicKey.toBuffer(), Buffer.from("authority")],
        program.programId
      );
      const [fakeShareMint] = PublicKey.findProgramAddressSync(
        [Buffer.from("vault"), fakeMint.publicKey.toBuffer(), Buffer.from("shares")],
        program.programId
      );
      const fakeAssetAccount = anchor.utils.token.associatedAddress({
        mint: fakeMint.publicKey,
        owner: fakeAuthority,
      });

      await program.methods
        .initializeVault(50, excessiveLeverageBps)
        .accounts({
          admin: admin.publicKey,
          assetMint: fakeMint.publicKey,
          vault: fakeVault,
          vaultAuthority: fakeAuthority,
          shareMint: fakeShareMint,
          vaultAssetAccount: fakeAssetAccount,
          systemProgram: SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
          rent: anchor.web3.SYSVAR_RENT_PUBKEY,
        })
        .signers([admin])
        .rpc();

      expect.fail("Should have failed on excessive leverage check");
    } catch (err: any) {
      expect(err.error?.errorCode?.code).to.equal("ExcessiveLeverageConfigured");
    }
  });

  it("3. Deposits base asset and mints 1:1 shares on initial liquidity", async () => {
    const depositAmount = new anchor.BN(500_000_000); // 500 USDC

    await program.methods
      .deposit(depositAmount)
      .accounts({
        user: user.publicKey,
        vault: vaultPda,
        vaultAuthority: vaultAuthorityPda,
        shareMint: shareMintPda,
        userTokenAccount: userTokenAccount,
        userShareAccount: userShareAccount,
        vaultAssetAccount: vaultAssetAccount,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([user])
      .rpc();

    const userShares = await getAccount(provider.connection, userShareAccount);
    const vaultToken = await getAccount(provider.connection, vaultAssetAccount);
    const vaultState = await program.account.vaultState.fetch(vaultPda);

    expect(Number(userShares.amount)).to.equal(500_000_000);
    expect(Number(vaultToken.amount)).to.equal(500_000_000);
    expect(vaultState.totalUnderlyingDeposited.toNumber()).to.equal(500_000_000);
  });

  it("4. Prevents unauthorized emergency pause from non-admin wallet", async () => {
    try {
      await program.methods
        .setPaused(true)
        .accounts({
          admin: user.publicKey, // User is not admin
          vault: vaultPda,
        })
        .signers([user])
        .rpc();

      expect.fail("Unauthorized pause should have been rejected");
    } catch (err: any) {
      expect(err.error?.errorCode?.code).to.equal("UnauthorizedAdmin");
    }
  });

  it("5. Admin triggers emergency circuit breaker; deposits are rejected", async () => {
    await program.methods
      .setPaused(true)
      .accounts({
        admin: admin.publicKey,
        vault: vaultPda,
      })
      .signers([admin])
      .rpc();

    const vaultState = await program.account.vaultState.fetch(vaultPda);
    expect(vaultState.isPaused).to.be.true;

    try {
      await program.methods
        .deposit(new anchor.BN(10_000_000))
        .accounts({
          user: user.publicKey,
          vault: vaultPda,
          vaultAuthority: vaultAuthorityPda,
          shareMint: shareMintPda,
          userTokenAccount: userTokenAccount,
          userShareAccount: userShareAccount,
          vaultAssetAccount: vaultAssetAccount,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([user])
        .rpc();

      expect.fail("Deposit must revert when paused");
    } catch (err: any) {
      expect(err.error?.errorCode?.code).to.equal("ProgramPaused");
    }

    // Unpause for subsequent tests
    await program.methods
      .setPaused(false)
      .accounts({
        admin: admin.publicKey,
        vault: vaultPda,
      })
      .signers([admin])
      .rpc();
  });

  it("6. Pro-rata withdrawal burns shares and returns base collateral", async () => {
    const sharesToBurn = new anchor.BN(200_000_000); // Burn 200 of 500 shares (40%)

    await program.methods
      .withdraw(sharesToBurn)
      .accounts({
        user: user.publicKey,
        vault: vaultPda,
        vaultAuthority: vaultAuthorityPda,
        shareMint: shareMintPda,
        userTokenAccount: userTokenAccount,
        userShareAccount: userShareAccount,
        vaultAssetAccount: vaultAssetAccount,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([user])
      .rpc();

    const userShares = await getAccount(provider.connection, userShareAccount);
    const vaultState = await program.account.vaultState.fetch(vaultPda);

    expect(Number(userShares.amount)).to.equal(300_000_000);
    expect(vaultState.totalUnderlyingDeposited.toNumber()).to.equal(300_000_000);
  });
});