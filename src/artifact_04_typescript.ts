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
import { Keypair, LAMPORTS_PER_SOL, PublicKey } from "@solana/web3.js";
import { expect } from "chai";

describe("delta-neutral-router integration tests", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.DeltaNeutralRouter as Program<DeltaNeutralRouter>;

  let usdcMint: PublicKey;
  let solMint: PublicKey;
  let admin = Keypair.generate();
  let user = Keypair.generate();
  let keeper = Keypair.generate();

  let userUsdcAccount: PublicKey;
  let userShareAccount: PublicKey;

  let strategyPda: PublicKey;
  let authorityPda: PublicKey;
  let usdcVaultPda: PublicKey;
  let shareMintPda: PublicKey;

  const vaultId = new anchor.BN(42);

  before(async () => {
    // Fund Admin, User, Keeper
    for (const kp of [admin, user, keeper]) {
      const sig = await provider.connection.requestAirdrop(kp.publicKey, 10 * LAMPORTS_PER_SOL);
      await provider.connection.confirmTransaction(sig);
    }

    // Initialize Mock SPL Tokens
    usdcMint = await createMint(provider.connection, admin, admin.publicKey, null, 6);
    solMint = await createMint(provider.connection, admin, admin.publicKey, null, 9);

    // Derive PDAs
    [strategyPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), vaultId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );
    [authorityPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("authority"), strategyPda.toBuffer()],
      program.programId
    );
    [usdcVaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("usdc_vault"), strategyPda.toBuffer()],
      program.programId
    );
    [shareMintPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("shares"), strategyPda.toBuffer()],
      program.programId
    );

    // Setup User USDC token account
    userUsdcAccount = await createAccount(provider.connection, user, usdcMint, user.publicKey);
    await mintTo(provider.connection, admin, usdcMint, userUsdcAccount, admin, 1_000_000_000); // 1,000 USDC

    // Create user share token holding account
    userShareAccount = await createAccount(provider.connection, user, shareMintPda, user.publicKey);
  });

  it("Initializes the strategy vault with verified boundary constraints", async () => {
    const mockMarginfiAccount = Keypair.generate().publicKey;
    const mockKaminoObligation = Keypair.generate().publicKey;

    await program.methods
      .initializeStrategy(vaultId, new anchor.BN(6000)) // 60% LTV
      .accounts({
        strategy: strategyPda,
        strategyAuthority: authorityPda,
        admin: admin.publicKey,
        usdcMint,
        solMint,
        usdcVault: usdcVaultPda,
        shareMint: shareMintPda,
        marginfiAccount: mockMarginfiAccount,
        kaminoObligation: mockKaminoObligation,
        systemProgram: anchor.web3.SystemProgram.programId,
        tokenProgram: TOKEN_PROGRAM_ID,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .signers([admin])
      .rpc();

    const vault = await program.account.strategyVault.fetch(strategyPda);
    expect(vault.vaultId.toNumber()).to.equal(42);
    expect(vault.targetLeverageBps.toNumber()).to.equal(6000);
    expect(vault.isRebalancing).to.be.false;
  });

  it("Executes a valid user deposit and mints exact LP share equivalents", async () => {
    const depositAmount = new anchor.BN(500_000_000); // 500 USDC
    const mockPythSol = Keypair.generate().publicKey;
    const mockPythUsdc = Keypair.generate().publicKey;

    await program.methods
      .deposit(depositAmount)
      .accounts({
        strategy: strategyPda,
        strategyAuthority: authorityPda,
        user: user.publicKey,
        userUsdc: userUsdcAccount,
        usdcVault: usdcVaultPda,
        shareMint: shareMintPda,
        userShares: userShareAccount,
        pythSolFeed: mockPythSol,
        pythUsdcFeed: mockPythUsdc,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([user])
      .rpc();

    const vaultUsdc = await getAccount(provider.connection, usdcVaultPda);
    expect(Number(vaultUsdc.amount)).to.equal(500_000_000);

    const userShares = await getAccount(provider.connection, userShareAccount);
    expect(Number(userShares.amount)).to.equal(500_000_000);
  });

  it("Rejects zero-token deposit attempts", async () => {
    try {
      await program.methods
        .deposit(new anchor.BN(0))
        .accounts({
          strategy: strategyPda,
          strategyAuthority: authorityPda,
          user: user.publicKey,
          userUsdc: userUsdcAccount,
          usdcVault: usdcVaultPda,
          shareMint: shareMintPda,
          userShares: userShareAccount,
          pythSolFeed: Keypair.generate().publicKey,
          pythUsdcFeed: Keypair.generate().publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([user])
        .rpc();
      expect.fail("Deposit of zero should have failed");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("ZeroDepositAmount");
    }
  });

  it("Allows user to withdraw capital proportional to burned shares", async () => {
    const burnAmount = new anchor.BN(250_000_000); // Burn 50% of shares

    await program.methods
      .withdraw(burnAmount)
      .accounts({
        strategy: strategyPda,
        strategyAuthority: authorityPda,
        user: user.publicKey,
        userShares: userShareAccount,
        shareMint: shareMintPda,
        usdcVault: usdcVaultPda,
        userUsdc: userUsdcAccount,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([user])
      .rpc();

    const userShares = await getAccount(provider.connection, userShareAccount);
    expect(Number(userShares.amount)).to.equal(250_000_000);

    const vaultUsdc = await getAccount(provider.connection, usdcVaultPda);
    expect(Number(vaultUsdc.amount)).to.equal(250_000_000);
  });
});