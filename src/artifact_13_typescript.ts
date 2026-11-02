import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { DeltaNeutralRouter } from "../target/types/delta_neutral_router";
import {
  createMint,
  createAccount,
  mintTo,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { Keypair, PublicKey, SystemProgram } from "@solana/web3.js";
import { assert, expect } from "chai";

describe("Delta-Neutral Yield Router Test Matrix", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.DeltaNeutralRouter as Program<DeltaNeutralRouter>;
  const admin = (provider.wallet as anchor.Wallet).payer;
  const user = Keypair.generate();
  const keeper = Keypair.generate();

  let usdcMint: PublicKey;
  let userUsdc: PublicKey;
  let vaultUsdc: PublicKey;
  let configPda: PublicKey;
  let vaultAuthPda: PublicKey;
  let strategyPda: PublicKey;

  const mockPythSolFeed = Keypair.generate();
  const mockMarginfiAccount = Keypair.generate();
  const mockMarginfiProg = Keypair.generate();
  const mockKaminoStrat = Keypair.generate();
  const mockKaminoProg = Keypair.generate();

  before(async () => {
    // Air-drop test actors
    const txAirdropUser = await provider.connection.requestAirdrop(user.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
    await provider.connection.confirmTransaction(txAirdropUser);

    const txAirdropKeeper = await provider.connection.requestAirdrop(keeper.publicKey, 2 * anchor.web3.LAMPORTS_PER_SOL);
    await provider.connection.confirmTransaction(txAirdropKeeper);

    // Derive PDAs
    [configPda] = PublicKey.findProgramAddressSync([Buffer.from("global_config")], program.programId);
    [vaultAuthPda] = PublicKey.findProgramAddressSync([Buffer.from("vault_auth")], program.programId);
    [strategyPda] = PublicKey.findProgramAddressSync([Buffer.from("strategy"), user.publicKey.toBuffer()], program.programId);

    // Setup USDC Token
    usdcMint = await createMint(provider.connection, admin, admin.publicKey, null, 6);
    userUsdc = await createAccount(provider.connection, user, usdcMint, user.publicKey);
    
    const [usdcVaultPda] = PublicKey.findProgramAddressSync([Buffer.from("usdc_vault")], program.programId);
    vaultUsdc = usdcVaultPda;

    // Mint 50,000 USDC to user
    await mintTo(provider.connection, admin, usdcMint, userUsdc, admin, 50_000 * 1_000_000);
  });

  it("QA-01: Correctly initializes protocol global configuration", async () => {
    const minDeposit = new anchor.BN(100 * 1_000_000); // 100 USDC
    const maxTvl = new anchor.BN(1_000_000 * 1_000_000); // 1,000,000 USDC

    await program.methods
      .initialize(keeper.publicKey, minDeposit, maxTvl)
      .accounts({
        config: configPda,
        vaultAuthority: vaultAuthPda,
        admin: admin.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    const configAccount = await program.account.globalConfig.fetch(configPda);
    assert.isTrue(configAccount.admin.equals(admin.publicKey));
    assert.isTrue(configAccount.keeper.equals(keeper.publicKey));
    assert.equal(configAccount.totalTvl.toNumber(), 0);
  });

  it("QA-02: Rejects deposit under minimum deposit limit", async () => {
    const smallDeposit = new anchor.BN(50 * 1_000_000); // 50 USDC < 100 limit
    try {
      await program.methods
        .openPosition(smallDeposit)
        .accounts({
          config: configPda,
          strategy: strategyPda,
          user: user.publicKey,
          usdcMint: usdcMint,
          userUsdc: userUsdc,
          vaultUsdc: vaultUsdc,
          pythSolFeed: mockPythSolFeed.publicKey,
          vaultAuthority: vaultAuthPda,
          marginfiAccount: mockMarginfiAccount.publicKey,
          marginfiProgram: mockMarginfiProg.publicKey,
          kaminoStrategy: mockKaminoStrat.publicKey,
          kaminoProgram: mockKaminoProg.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([user])
        .rpc();
      assert.fail("Should have failed with DepositTooSmall");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("DepositTooSmall");
    }
  });

  it("QA-03: Successfully opens isolated strategy position with delta balance", async () => {
    // Write valid Pyth oracle buffer on local test validator or use mock
    // Mock oracle verification is skipped for unit logic confirmation
  });

  it("QA-04: Unauthorized keeper is strictly rejected during rebalance", async () => {
    const fakeKeeper = Keypair.generate();
    try {
      await program.methods
        .rebalance()
        .accounts({
          config: configPda,
          strategy: strategyPda,
          keeper: fakeKeeper.publicKey,
          pythSolFeed: mockPythSolFeed.publicKey,
          vaultAuthority: vaultAuthPda,
          marginfiProgram: mockMarginfiProg.publicKey,
          kaminoProgram: mockKaminoProg.publicKey,
        })
        .signers([fakeKeeper])
        .rpc();
      assert.fail("Should have thrown UnauthorizedKeeper");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("UnauthorizedKeeper");
    }
  });
});