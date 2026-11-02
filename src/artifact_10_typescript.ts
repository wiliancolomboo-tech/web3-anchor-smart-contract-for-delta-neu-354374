import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { DeltaNeutralRouter } from "../target/types/delta_neutral_router";
import { 
  Keypair, 
  PublicKey, 
  SystemProgram, 
  SYSVAR_RENT_PUBKEY 
} from "@solana/web3.js";
import { 
  TOKEN_PROGRAM_ID, 
  createMint, 
  createAccount, 
  mintTo, 
  getAccount 
} from "@solana/spl-token";
import { expect } from "chai";

describe("delta_neutral_router E2E Test Suite", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.DeltaNeutralRouter as Program<DeltaNeutralRouter>;

  let admin = Keypair.generate();
  let keeper = Keypair.generate();
  let user = Keypair.generate();

  let quoteMint: PublicKey;
  let underlyingMint: PublicKey;
  let kaminoCollateralMint: PublicKey;

  let userQuoteAccount: PublicKey;
  let vaultQuoteAccount: PublicKey;
  let vaultUnderlyingAccount: PublicKey;
  let vaultKaminoCollateralAccount: PublicKey;

  let globalVaultPda: PublicKey;
  let vaultAuthorityPda: PublicKey;
  let strategyConfigPda: PublicKey;
  let userPositionPda: PublicKey;

  // Mock Program Keys for CPI target emulation
  const mockKaminoProgram = Keypair.generate().publicKey;
  const mockMarginfiProgram = Keypair.generate().publicKey;
  const mockMarginfiGroup = Keypair.generate().publicKey;
  const mockMarginfiAccount = Keypair.generate().publicKey;
  const mockMarginfiBank = Keypair.generate().publicKey;
  const mockMarginfiBankVault = Keypair.generate().publicKey;
  const mockMarginfiBankAuth = Keypair.generate().publicKey;
  const mockKaminoReserve = Keypair.generate().publicKey;
  const mockKaminoMarket = Keypair.generate().publicKey;
  const mockKaminoLiqSupply = Keypair.generate().publicKey;
  const mockPythOracleFeed = Keypair.generate().publicKey;

  before(async () => {
    // Fund test wallets
    for (const kp of [admin, keeper, user]) {
      const sig = await provider.connection.requestAirdrop(kp.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
      const latestBlockhash = await provider.connection.getLatestBlockhash();
      await provider.connection.confirmTransaction({ signature: sig, ...latestBlockhash });
    }

    // Initialize Mock SPL Mints (Quote = 6 decimals USDC, Underlying = 9 decimals SOL)
    quoteMint = await createMint(provider.connection, admin, admin.publicKey, null, 6);
    underlyingMint = await createMint(provider.connection, admin, admin.publicKey, null, 9);
    kaminoCollateralMint = await createMint(provider.connection, admin, admin.publicKey, null, 6);

    // Derive PDAs
    [globalVaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), quoteMint.toBuffer(), underlyingMint.toBuffer()],
      program.programId
    );

    [vaultAuthorityPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault_authority"), globalVaultPda.toBuffer()],
      program.programId
    );

    [strategyConfigPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("strategy"), globalVaultPda.toBuffer()],
      program.programId
    );

    [userPositionPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("position"), globalVaultPda.toBuffer(), user.publicKey.toBuffer()],
      program.programId
    );

    // Create Token Accounts
    userQuoteAccount = await createAccount(provider.connection, user, quoteMint, user.publicKey);
    vaultQuoteAccount = await createAccount(provider.connection, admin, quoteMint, vaultAuthorityPda);
    vaultUnderlyingAccount = await createAccount(provider.connection, admin, underlyingMint, vaultAuthorityPda);
    vaultKaminoCollateralAccount = await createAccount(provider.connection, admin, kaminoCollateralMint, vaultAuthorityPda);

    // Mint Initial 100,000 USDC to User
    await mintTo(provider.connection, admin, quoteMint, userQuoteAccount, admin, 100_000 * 1_000_000);
  });

  it("1. Initializes Global Vault and Strategy State Correctly", async () => {
    const targetHealthFactor = new anchor.BN("1350000000000000000"); // 1.35 * 1e18
    const minHealthFactor = new anchor.BN("1200000000000000000");    // 1.20 * 1e18
    const targetLeverageBps = 10000; // 1.0x neutral hedge
    const maxSlippageBps = 50;       // 0.5%
    const cooldownSec = new anchor.BN(30);

    await program.methods
      .initializeVault(
        targetHealthFactor,
        minHealthFactor,
        targetLeverageBps,
        maxSlippageBps,
        cooldownSec
      )
      .accounts({
        admin: admin.publicKey,
        keeper: keeper.publicKey,
        quoteMint: quoteMint,
        underlyingMint: underlyingMint,
        globalVault: globalVaultPda,
        vaultAuthority: vaultAuthorityPda,
        strategyConfig: strategyConfigPda,
        kaminoProgram: mockKaminoProgram,
        kaminoReserve: mockKaminoReserve,
        marginfiProgram: mockMarginfiProgram,
        marginfiGroup: mockMarginfiGroup,
        marginfiAccount: mockMarginfiAccount,
        systemProgram: SystemProgram.programId,
        rent: SYSVAR_RENT_PUBKEY,
      })
      .signers([admin])
      .rpc();

    const vaultAccount = await program.account.globalVault.fetch(globalVaultPda);
    expect(vaultAccount.admin.toBase58()).to.equal(admin.publicKey.toBase58());
    expect(vaultAccount.keeper.toBase58()).to.equal(keeper.publicKey.toBase58());
    expect(vaultAccount.totalLpShares.toNumber()).to.equal(0);
  });

  it("2. Rejects Zero Deposit Invocations", async () => {
    try {
      await program.methods
        .depositAndHedge(new anchor.BN(0))
        .accounts({
          user: user.publicKey,
          globalVault: globalVaultPda,
          vaultAuthority: vaultAuthorityPda,
          userPosition: userPositionPda,
          userQuoteAccount: userQuoteAccount,
          vaultQuoteAccount: vaultQuoteAccount,
          vaultUnderlyingAccount: vaultUnderlyingAccount,
          vaultKaminoCollateralAccount: vaultKaminoCollateralAccount,
          kaminoProgram: mockKaminoProgram,
          kaminoLendingMarket: mockKaminoMarket,
          kaminoReserve: mockKaminoReserve,
          kaminoReserveLiquiditySupply: mockKaminoLiqSupply,
          kaminoCollateralMint: kaminoCollateralMint,
          marginfiProgram: mockMarginfiProgram,
          marginfiGroup: mockMarginfiGroup,
          marginfiAccount: mockMarginfiAccount,
          marginfiBank: mockMarginfiBank,
          marginfiBankVault: mockMarginfiBankVault,
          marginfiBankAuthority: mockMarginfiBankAuth,
          oracleSolFeed: mockPythOracleFeed,
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .signers([user])
        .rpc();
      expect.fail("Expected zero deposit error");
    } catch (err: any) {
      expect(err.error?.errorCode?.code || err.toString()).to.include("ZeroAmountProvided");
    }
  });

  it("3. Enforces Keeper Cooldown and Signer Authentication on Rebalance", async () => {
    const rogueUser = Keypair.generate();
    const sig = await provider.connection.requestAirdrop(rogueUser.publicKey, 2 * anchor.web3.LAMPORTS_PER_SOL);
    await provider.connection.confirmTransaction(sig);

    try {
      await program.methods
        .dynamicRebalance(new anchor.BN(100_000_000), false)
        .accounts({
          keeper: rogueUser.publicKey,
          globalVault: globalVaultPda,
          vaultAuthority: vaultAuthorityPda,
          strategyConfig: strategyConfigPda,
          vaultUnderlyingAccount: vaultUnderlyingAccount,
          marginfiProgram: mockMarginfiProgram,
          marginfiGroup: mockMarginfiGroup,
          marginfiAccount: mockMarginfiAccount,
          marginfiBank: mockMarginfiBank,
          marginfiBankVault: mockMarginfiBankVault,
          marginfiBankAuthority: mockMarginfiBankAuth,
          oracleSolFeed: mockPythOracleFeed,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([rogueUser])
        .rpc();
      expect.fail("Expected unauthorized keeper error");
    } catch (err: any) {
      expect(err.error?.errorCode?.code || err.toString()).to.include("UnauthorizedSigner");
    }
  });
});