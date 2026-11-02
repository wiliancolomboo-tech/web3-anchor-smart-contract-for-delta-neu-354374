import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { DeltaNeutralRouter } from "../target/types/delta_neutral_router";
import { Connection, Keypair, PublicKey } from "@solana/web3.js";
import { getPriceData } from "./oracleUtils";
import winston from "winston";

const logger = winston.createLogger({
  level: "info",
  format: winston.format.combine(
    winston.format.timestamp(),
    winston.format.json()
  ),
  transports: [new winston.transports.Console()],
});

export class DeltaNeutralKeeper {
  private program: Program<DeltaNeutralRouter>;
  private connection: Connection;
  private keeperKeypair: Keypair;
  private vaultPubkey: PublicKey;

  constructor(
    program: Program<DeltaNeutralRouter>,
    connection: Connection,
    keeperKeypair: Keypair,
    vaultPubkey: PublicKey
  ) {
    this.program = program;
    this.connection = connection;
    this.keeperKeypair = keeperKeypair;
    this.vaultPubkey = vaultPubkey;
  }

  public async runLoop(intervalMs: number = 10_000): Promise<void> {
    logger.info("Initializing Delta-Neutral Automated Keeper Worker...");

    setInterval(async () => {
      try {
        await this.evaluateAndRebalance();
      } catch (err) {
        logger.error("Error during keeper execution iteration:", { error: err });
      }
    }, intervalMs);
  }

  private async evaluateAndRebalance(): Promise<void> {
    const strategy = await this.program.account.strategyVault.fetch(this.vaultPubkey);
    if (strategy.isRebalancing) {
      logger.warn("Vault locked in transient state. Skipping tick.");
      return;
    }

    const { solPrice, confidenceRatio } = await getPriceData(this.connection);
    if (confidenceRatio > 0.005) {
      logger.warn("Confidence interval too wide. Skipping rebalance.", { confidenceRatio });
      return;
    }

    // Delta-neutral balance check:
    // Long Kamino Position vs. Short Marginfi Debt
    const currentShortExposure = await this.fetchMarginfiDebt(strategy.marginfiAccount);
    const currentLongExposure = await this.fetchKaminoCollateral(strategy.kaminoObligation);

    const deltaExposure = Math.abs(currentLongExposure - currentShortExposure);
    const tolerance = 0.02 * currentLongExposure; // 2% drift tolerance

    if (deltaExposure > tolerance) {
      logger.info("Delta drift detected. Firing atomic rebalance instruction...", {
        currentLongExposure,
        currentShortExposure,
        deltaExposure,
      });

      const tx = await this.program.methods
        .executeRebalance(
          new anchor.BN(10_000_000), // Collateral allocation (USDC)
          new anchor.BN(50_000_000)  // Hedged borrow (Lamports)
        )
        .accounts({
          strategy: this.vaultPubkey,
          keeper: this.keeperKeypair.publicKey,
          // Account context omitted for brevity
        })
        .signers([this.keeperKeypair])
        .rpc();

      logger.info("Rebalance successfully executed.", { signature: tx });
    }
  }

  private async fetchMarginfiDebt(_account: PublicKey): Promise<number> {
    return 10.5; // Mock debt tracking
  }

  private async fetchKaminoCollateral(_obligation: PublicKey): Promise<number> {
    return 10.5; // Mock collateral tracking
  }
}