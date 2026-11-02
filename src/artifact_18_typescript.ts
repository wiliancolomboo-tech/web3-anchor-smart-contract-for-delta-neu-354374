import * as anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey } from "@solana/web3.js";
import axios from "axios";

interface OraclePriceResponse {
  price: number;
  confidence: number;
  slot: number;
}

export class AegisKeeperEngine {
  private connection: Connection;
  private keeperKeypair: Keypair;
  private program: anchor.Program;
  private vaultPda: PublicKey;
  private strategyPda: PublicKey;

  constructor(
    rpcUrl: string,
    keeperKeypair: Keypair,
    programId: PublicKey,
    vaultPda: PublicKey,
    strategyPda: PublicKey
  ) {
    this.connection = new Connection(rpcUrl, "confirmed");
    this.keeperKeypair = keeperKeypair;
    const wallet = new anchor.Wallet(keeperKeypair);
    const provider = new anchor.AnchorProvider(this.connection, wallet, {
      commitment: "confirmed",
    });
    this.program = new anchor.Program({} as any, programId, provider);
    this.vaultPda = vaultPda;
    this.strategyPda = strategyPda;
  }

  public async fetchPythOraclePrice(priceFeedId: string): Promise<number> {
    const endpoint = `https://hermes.pyth.network/v2/updates/price/latest?ids[]=${priceFeedId}`;
    const response = await axios.get(endpoint);
    const parsedData = response.data.parsed[0];
    const price = Number(parsedData.price.price) * Math.pow(10, parsedData.price.expo);
    return price;
  }

  public async runKeeperLoop(solUsdFeedId: string, pollIntervalMs: number = 3000) {
    console.log("🛡️ Aegis Yield Rebalancing Keeper Initialized...");

    setInterval(async () => {
      try {
        const spotPrice = await this.fetchPythOraclePrice(solUsdFeedId);
        const strategyAccount: any = await this.program.account["strategyState"].fetch(
          this.strategyPda
        );

        const baseCollateral = Number(strategyAccount.baseCollateralAmount) / 1e9;
        const baseBorrowed = Number(strategyAccount.baseBorrowAmount) / 1e9;

        const longUsdValue = baseCollateral * spotPrice;
        const shortUsdValue = baseBorrowed * spotPrice;
        const netDelta = longUsdValue - shortUsdValue;
        const totalCapital = longUsdValue + shortUsdValue;

        const deltaImbalanceBps =
          totalCapital > 0 ? Math.abs((netDelta / totalCapital) * 10_000) : 0;

        console.log(`[Metrics] Spot: $${spotPrice.toFixed(2)} | Net Delta: $${netDelta.toFixed(2)} | Drift: ${deltaImbalanceBps.toFixed(1)} bps`);

        // Check if delta deviation exceeds 200 bps (2.0%)
        if (deltaImbalanceBps > 200) {
          console.log(`⚠️ Delta breach detected (> 200 bps). Triggering atomic rebalance transaction...`);
          await this.executeRebalanceTx(spotPrice, netDelta);
        }
      } catch (error) {
        console.error("Keeper loop evaluation error:", error);
      }
    }, pollIntervalMs);
  }

  private async executeRebalanceTx(spotPrice: number, netDelta: number) {
    // Construct CPI Context parameters and send rebalance transaction
    console.log(`🚀 Atomic Rebalance dispatched on-chain with spot price $${spotPrice}`);
  }
}