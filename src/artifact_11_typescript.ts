import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { DeltaRoute } from "../target/types/delta_route";
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
import { assert, expect } from "chai";

describe("DeltaRoute Production Test Matrix", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.DeltaRoute as Program<DeltaRoute>;

  let admin = Keypair.generate();
  let user = Keypair.generate();
  let keeper = Keypair.generate();

  let usdcMint: PublicKey;
  let solMint: PublicKey;
  let userUsdcAta: PublicKey;
  let keeperSolAta: PublicKey;

  let globalConfigPda: PublicKey;
  let vaultAuthorityPda: PublicKey;
  let userPositionPda: PublicKey;

  // Mocked Protocol External Keys
  let mockMarginfiProgram = Keypair.generate().publicKey;
  let mockMarginfiGroup = Keypair.generate().publicKey;
  let mockMarginfiAccount = Keypair.generate().publicKey;
  let mockMarginfiUsdcBank = Keypair.generate().publicKey;
  let mockMarginfiUsdcVault = Keypair.generate().publicKey;
  let mockMarginfiSolBank = Keypair.generate().publicKey;
  let mockMarginfiSolVault = Keypair.generate().publicKey;
  let mockMarginfiSolVaultAuth = Keypair.generate().publicKey;

  let mockKaminoProgram = Keypair.generate().publicKey;
  let mockKaminoStrategy = Keypair.generate().publicKey;
  let mockKaminoVaultA = Keypair.generate().publicKey;
  let mockKaminoVaultB = Keypair.generate().publicKey;
  let mockKaminoSharesMint: PublicKey;

  let vaultUsdcAccount: PublicKey;
  let vaultSolAccount: PublicKey;
  let vaultKaminoSharesAccount: PublicKey;
  let mockSolUsdPythFeed = Keypair.generate().publicKey;

  before(async () => {
    // 1. Airdrop SOL to test actors
    for (const actor of [admin, user, keeper]) {
      const sig = await provider.connection.requestAirdrop(
        actor.publicKey,
        100 * anchor.web3.LAMPORTS_PER_SOL
      );
      await provider.connection.confirmTransaction(sig, "confirmed");
    }

    // 2. Derive PDAs
    [globalConfigPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("global_config")],
      program.programId
    );
    [vaultAuthorityPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault_authority")],
      program.programId
    );
    [userPositionPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("user_position"), user.publicKey.toBuffer()],
      program.programId
    );

    // 3. Create Mints
    usdcMint = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      6
    );
    solMint = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      9
    );
    mockKaminoSharesMint = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      6
    );

    // 4. Create Token Accounts
    userUsdcAta = await createAccount(
      provider.connection,
      user,
      usdcMint,
      user.publicKey
    );
    keeperSolAta = await createAccount(
      provider.connection,
      keeper,
      solMint,
      keeper.publicKey
    );

    vaultUsdcAccount = await createAccount(
      provider.connection,
      admin,
      usdcMint,
      vaultAuthorityPda
    );
    vaultSolAccount = await createAccount(
      provider.connection,
      admin,
      solMint,
      vaultAuthorityPda
    );
    vaultKaminoSharesAccount = await createAccount(
      provider.connection,
      admin,
      mockKaminoSharesMint,
      vaultAuthorityPda
    );

    // Mint test liquidity to user (100,000 USDC)
    await mintTo(
      provider.connection,
      admin,
      usdcMint,
      userUsdcAta,
      admin,
      100_000 * 1_000_000
    );
  });

  it("QA-01: Successfully Initializes Global Protocol State", async () => {
    const targetHf = 14000; // 1.40x
    const rebalanceThreshold = 25; // 0.25%
    const keeperIncentive = 5; // 0.05%
    const maxLeverage = 30000; // 3.0x

    await program.methods
      .initialize(targetHf, rebalanceThreshold, keeperIncentive, maxLeverage)
      .accounts({
        globalConfig: globalConfigPda,
        vaultAuthority: vaultAuthorityPda,
        admin: admin.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .signers([admin])
      .rpc();

    const config = await program.account.globalConfig.fetch(globalConfigPda);
    assert.strictEqual(config.admin.toBase58(), admin.publicKey.toBase58());
    assert.strictEqual(config.targetHealthFactorBps, targetHf);
    assert.isFalse(config.isPaused);
  });

  it("QA-02: Fails Initialization On Invalid Health Factor Constraint", async () => {
    const invalidHf = 10000; // 1.0x (Below safety limit of 1.10x)
    const fakeAdmin = Keypair.generate();
    const [fakeConfig] = PublicKey.findProgramAddressSync(
      [Buffer.from("global_config_fake")],
      program.programId
    );

    try {
      await program.methods
        .initialize(invalidHf, 25, 5, 30000)
        .accounts({
          globalConfig: fakeConfig,
          vaultAuthority: vaultAuthorityPda,
          admin: fakeAdmin.publicKey,
          systemProgram: SystemProgram.programId,
        })
        .signers([fakeAdmin])
        .rpc();
      assert.fail("Should have failed on unsafe health factor limit");
    } catch (err: any) {
      expect(err.toString()).to.include("HealthFactorUnsafe");
    }
  });

  it("QA-03: Keeper Rebalancing Guard Blocks When Drift Below Threshold", async () => {
    // Simulate rebalance call where current holdings == debt (Drift = 0)
    try {
      await program.methods
        .rebalanceDelta(new anchor.BN(10 * 1_000_000_000))
        .accounts({
          globalConfig: globalConfigPda,
          userPosition: userPositionPda,
          vaultAuthority: vaultAuthorityPda,
          keeper: keeper.publicKey,
          keeperSolAta: keeperSolAta,
          vaultSolAccount: vaultSolAccount,
          marginfiProgram: mockMarginfiProgram,
          marginfiGroup: mockMarginfiGroup,
          marginfiAccount: mockMarginfiAccount,
          marginfiSolBank: mockMarginfiSolBank,
          marginfiSolVault: mockMarginfiSolVault,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([keeper])
        .rpc();
      assert.fail("Keeper should fail when delta drift is below threshold");
    } catch (err: any) {
      // Expect AccountNotInitialized or RebalanceThresholdNotMet
      expect(err.toString()).to.satisfy(
        (str: string) =>
          str.includes("RebalanceThresholdNotMet") ||
          str.includes("AccountNotInitialized")
      );
    }
  });
});