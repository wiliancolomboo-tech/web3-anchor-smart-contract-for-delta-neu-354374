import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { NeutraYield } from "../target/types/neutra_yield";
import {
  Keypair,
  PublicKey,
  SystemProgram,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { expect } from "chai";

describe("NeutraYield: Delta-Neutral Engine QA Matrix", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.NeutraYield as Program<NeutraYield>;

  let admin = Keypair.generate();
  let keeper = Keypair.generate();
  let depositor = Keypair.generate();

  let mint: PublicKey;
  let depositorTokenAccount: PublicKey;
  let vaultTokenAccount: PublicKey;
  let globalConfigPda: PublicKey;
  let strategyVaultPda: PublicKey;
  let userPositionPda: PublicKey;

  before(async () => {
    // Airdrop SOL to test actors
    for (const actor of [admin, keeper, depositor]) {
      const sig = await provider.connection.requestAirdrop(
        actor.publicKey,
        10 * anchor.web3.LAMPORTS_PER_SOL
      );
      await provider.connection.confirmTransaction(sig);
    }

    // Initialize mock underlying token (e.g. JitoSOL or USDC)
    mint = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      6
    );

    [globalConfigPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("global-config")],
      program.programId
    );

    [strategyVaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("strategy-vault")],
      program.programId
    );

    [vaultTokenAccount] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault-token-account"), mint.toBuffer()],
      program.programId
    );

    [userPositionPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("user-position"), depositor.publicKey.toBuffer()],
      program.programId
    );

    depositorTokenAccount = await createAccount(
      provider.connection,
      depositor,
      mint,
      depositor.publicKey
    );

    // Mint 10,000 units to depositor
    await mintTo(
      provider.connection,
      admin,
      mint,
      depositorTokenAccount,
      admin,
      10_000_000_000
    );
  });

  it("1. Successfully initializes the strategy parameters", async () => {
    const targetLtv = 5000; // 50%
    const rebalanceThreshold = 150; // 1.5%
    const performanceFee = 1000; // 10%

    await program.methods
      .initializeStrategy(targetLtv, rebalanceThreshold, performanceFee)
      .accounts({
        globalConfig: globalConfigPda,
        strategyVault: strategyVaultPda,
        underlyingMint: mint,
        vaultTokenAccount: vaultTokenAccount,
        keeper: keeper.publicKey,
        admin: admin.publicKey,
        systemProgram: SystemProgram.programId,
        tokenProgram: TOKEN_PROGRAM_ID,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .signers([admin])
      .rpc();

    const configAccount = await program.account.globalConfig.fetch(globalConfigPda);
    expect(configAccount.admin.toBase58()).to.equal(admin.publicKey.toBase58());
    expect(configAccount.keeper.toBase58()).to.equal(keeper.publicKey.toBase58());
    expect(configAccount.targetLtvBps).to.equal(targetLtv);
    expect(configAccount.rebalanceThresholdBps).to.equal(rebalanceThreshold);
    expect(configAccount.isPaused).to.be.false;
  });

  it("2. User deposits collateral into the delta-neutral engine", async () => {
    const depositAmount = new anchor.BN(2_000_000_000); // 2,000 tokens

    await program.methods
      .deposit(depositAmount)
      .accounts({
        globalConfig: globalConfigPda,
        strategyVault: strategyVaultPda,
        userPosition: userPositionPda,
        vaultTokenAccount: vaultTokenAccount,
        depositorTokenAccount: depositorTokenAccount,
        depositor: depositor.publicKey,
        systemProgram: SystemProgram.programId,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([depositor])
      .rpc();

    const position = await program.account.userPosition.fetch(userPositionPda);
    expect(position.depositedAmount.toString()).to.equal(depositAmount.toString());

    const vaultAccount = await getAccount(provider.connection, vaultTokenAccount);
    expect(vaultAccount.amount.toString()).to.equal(depositAmount.toString());
  });

  it("3. Keeper cannot trigger rebalance when drift is below tolerance", async () => {
    const spotPrice = new anchor.BN(140_000_000);
    const collateralVal = new anchor.BN(100_000_000);
    const borrowVal = new anchor.BN(99_500_000); // 0.5% drift (50 bps) < 150 bps

    try {
      await program.methods
        .rebalance(spotPrice, collateralVal, borrowVal)
        .accounts({
          globalConfig: globalConfigPda,
          strategyVault: strategyVaultPda,
          signer: keeper.publicKey,
        })
        .signers([keeper])
        .rpc();
      expect.fail("Should have failed with DeltaWithinTolerance error");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("DeltaWithinTolerance");
    }
  });

  it("4. Keeper executes rebalance when drift exceeds threshold", async () => {
    const spotPrice = new anchor.BN(140_000_000);
    const collateralVal = new anchor.BN(100_000_000);
    const borrowVal = new anchor.BN(95_000_000); // 5% drift (500 bps) > 150 bps

    await program.methods
      .rebalance(spotPrice, collateralVal, borrowVal)
      .accounts({
        globalConfig: globalConfigPda,
        strategyVault: strategyVaultPda,
        signer: keeper.publicKey,
      })
      .signers([keeper])
      .rpc();

    const vault = await program.account.strategyVault.fetch(strategyVaultPda);
    expect(vault.lastRecordedDriftBps).to.equal(500);
  });

  it("5. Emergency freeze halts strategy operations", async () => {
    await program.methods
      .setEmergencyPause(true)
      .accounts({
        globalConfig: globalConfigPda,
        admin: admin.publicKey,
      })
      .signers([admin])
      .rpc();

    const depositAmount = new anchor.BN(500_000_000);

    try {
      await program.methods
        .deposit(depositAmount)
        .accounts({
          globalConfig: globalConfigPda,
          strategyVault: strategyVaultPda,
          userPosition: userPositionPda,
          vaultTokenAccount: vaultTokenAccount,
          depositorTokenAccount: depositorTokenAccount,
          depositor: depositor.publicKey,
          systemProgram: SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([depositor])
        .rpc();
      expect.fail("Should fail as strategy is paused");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("StrategyPaused");
    }

    // Unpause for clean test teardown
    await program.methods
      .setEmergencyPause(false)
      .accounts({
        globalConfig: globalConfigPda,
        admin: admin.publicKey,
      })
      .signers([admin])
      .rpc();
  });

  it("6. Depositor withdraws capital safely", async () => {
    const withdrawAmount = new anchor.BN(1_000_000_000);

    await program.methods
      .withdraw(withdrawAmount)
      .accounts({
        globalConfig: globalConfigPda,
        strategyVault: strategyVaultPda,
        userPosition: userPositionPda,
        vaultTokenAccount: vaultTokenAccount,
        depositorTokenAccount: depositorTokenAccount,
        depositor: depositor.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([depositor])
      .rpc();

    const position = await program.account.userPosition.fetch(userPositionPda);
    expect(position.depositedAmount.toString()).to.equal("1000000000");
  });
});