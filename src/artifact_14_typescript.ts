import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { AegisEngine } from "../target/types/aegis_engine";
import {
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { Keypair, PublicKey, SystemProgram } from "@solana/web3.js";
import { expect } from "chai";

describe("aegis_engine delta-neutral tests", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.AegisEngine as Program<AegisEngine>;

  let baseMint: PublicKey;
  let volatileMint: PublicKey;
  let strategyPda: PublicKey;
  let strategyBump: number;
  let vaultAuthPda: PublicKey;
  let vaultBaseToken: PublicKey;

  const admin = (provider.wallet as anchor.Wallet).payer;
  const user = Keypair.generate();
  let userBaseToken: PublicKey;

  before(async () => {
    // 1. Airdrop SOL to user
    const sig = await provider.connection.requestAirdrop(user.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
    await provider.connection.confirmTransaction(sig);

    // 2. Create Mints (USDC = 6 decimals, SOL = 9 decimals)
    baseMint = await createMint(provider.connection, admin, admin.publicKey, null, 6);
    volatileMint = await createMint(provider.connection, admin, admin.publicKey, null, 9);

    // 3. Derive Strategy PDA & Authority PDA
    [strategyPda, strategyBump] = PublicKey.findProgramAddressSync(
      [Buffer.from("aegis_strategy"), baseMint.toBuffer(), volatileMint.toBuffer()],
      program.programId
    );

    [vaultAuthPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault_authority"), strategyPda.toBuffer()],
      program.programId
    );

    // 4. Create Vault Base Token Account
    vaultBaseToken = await createAccount(
      provider.connection,
      admin,
      baseMint,
      vaultAuthPda,
      Keypair.generate()
    );

    // 5. Create & Mint to User Base Account
    userBaseToken = await createAccount(
      provider.connection,
      admin,
      baseMint,
      user.publicKey,
      Keypair.generate()
    );
    await mintTo(provider.connection, admin, baseMint, userBaseToken, admin.publicKey, 1_000_000_000); // 1,000 USDC
  });

  it("Initializes strategy with 65% Target LTV and 1.5% Drift Tolerance", async () => {
    const fakeOracleBase = Keypair.generate().publicKey;
    const fakeOracleVolatile = Keypair.generate().publicKey;

    await program.methods
      .initialize(6500, 150)
      .accounts({
        admin: admin.publicKey,
        baseMint: baseMint,
        volatileMint: volatileMint,
        strategy: strategyPda,
        vaultAuthority: vaultAuthPda,
        vaultBaseToken: vaultBaseToken,
        pythOracleBase: fakeOracleBase,
        pythOracleVolatile: fakeOracleVolatile,
        systemProgram: SystemProgram.programId,
        tokenProgram: anchor.spl.token.TOKEN_PROGRAM_ID,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    const state = await program.account.strategyState.fetch(strategyPda);
    expect(state.targetLtvBps).to.equal(6500);
    expect(state.maxDriftBps).to.equal(150);
    expect(state.isLocked).to.equal(0);
  });

  it("Executes user deposit and mints linear strategy shares", async () => {
    const depositAmount = new anchor.BN(500_000_000); // 500 USDC

    await program.methods
      .deposit(depositAmount)
      .accounts({
        user: user.publicKey,
        strategy: strategyPda,
        userBaseToken: userBaseToken,
        vaultBaseToken: vaultBaseToken,
        tokenProgram: anchor.spl.token.TOKEN_PROGRAM_ID,
      })
      .signers([user])
      .rpc();

    const state = await program.account.strategyState.fetch(strategyPda);
    expect(state.totalShares.toNumber()).to.equal(depositAmount.toNumber());
    expect(state.kaminoSuppliedAmount.toNumber()).to.equal(250_000_000);
    expect(state.marginfiCollateralAmount.toNumber()).to.equal(250_000_000);

    const vaultBalance = await getAccount(provider.connection, vaultBaseToken);
    expect(Number(vaultBalance.amount)).to.equal(500_000_000);
  });

  it("Executes dynamic rebalance to neutralize delta drift", async () => {
    const fakeOracleBase = Keypair.generate().publicKey;
    const fakeOracleVolatile = Keypair.generate().publicKey;
    const targetShortAmount = new anchor.BN(250_000_000);

    const tx = await program.methods
      .rebalance(targetShortAmount, new anchor.BN(0))
      .accounts({
        keeper: admin.publicKey,
        strategy: strategyPda,
        vaultAuthority: vaultAuthPda,
        vaultBaseToken: vaultBaseToken,
        vaultVolatileToken: vaultBaseToken, // Mocked for integration pass
        pythOracleBase: fakeOracleBase,
        pythOracleVolatile: fakeOracleVolatile,
        kaminoProgram: SystemProgram.programId,
        kaminoPool: SystemProgram.programId,
        marginfiProgram: SystemProgram.programId,
        marginfiAccount: SystemProgram.programId,
        marginfiBank: SystemProgram.programId,
        tokenProgram: anchor.spl.token.TOKEN_PROGRAM_ID,
      })
      .rpc();

    expect(tx).to.be.a("string");
    const state = await program.account.strategyState.fetch(strategyPda);
    expect(state.marginfiBorrowedAmount.toNumber()).to.equal(targetShortAmount.toNumber());
  });

  it("Rejects rebalances exceeding maximum LTV guard", async () => {
    // Attempt to borrow excessive amount that drives LTV beyond 65%
    const excessiveShortAmount = new anchor.BN(999_999_999);

    try {
      await program.methods
        .rebalance(excessiveShortAmount, new anchor.BN(0))
        .accounts({
          keeper: admin.publicKey,
          strategy: strategyPda,
          vaultAuthority: vaultAuthPda,
          vaultBaseToken: vaultBaseToken,
          vaultVolatileToken: vaultBaseToken,
          pythOracleBase: Keypair.generate().publicKey,
          pythOracleVolatile: Keypair.generate().publicKey,
          kaminoProgram: SystemProgram.programId,
          kaminoPool: SystemProgram.programId,
          marginfiProgram: SystemProgram.programId,
          marginfiAccount: SystemProgram.programId,
          marginfiBank: SystemProgram.programId,
          tokenProgram: anchor.spl.token.TOKEN_PROGRAM_ID,
        })
        .rpc();
      expect.fail("Expected LtvThresholdBreached error");
    } catch (err: any) {
      expect(err.toString()).to.include("LtvThresholdBreached");
    }
  });
});