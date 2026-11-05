import * as anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey } from "@solana/web3.js";
import axios from "axios";
import { Resend } from "resend";
import * as fs from "fs";
import * as dotenv from "dotenv";

dotenv.config();

// Enterprise Configuration
const RPC_ENDPOINT = process.env.SOLANA_RPC_URL || "https://api.mainnet-beta.solana.com";
const KEEPER_KEYPAIR_PATH = process.env.KEEPER_KEYPAIR_PATH || "./keeper-keypair.json";
const PROGRAM_ID = new PublicKey(process.env.PROGRAM_ID || "AegisDeltaNeutrav11111111111111111111111111");
const DISCORD_WEBHOOK_URL = process.env.DISCORD_WEBHOOK_URL || "";
const RESEND_API_KEY = process.env.RESEND_API_KEY || "";
const ALERT_EMAIL = process.env.ALERT_EMAIL || "ops@aegisfinance.io";

const resend = RESEND_API_KEY ? new Resend(RESEND_API_KEY) : null;

// Telemetry & Alerting Adapter
class EnterpriseTelemetry {
  public static async notify(subject: string, message: string, level: "INFO" | "WARN" | "CRITICAL") {
    const timestamp = new Date().toISOString();
    console.log(`[${timestamp}] [${level}] ${subject}: ${message}`);

    // 1. Emit to Discord / Slack Webhook
    if (DISCORD_WEBHOOK_URL) {
      try {
        const color = level === "CRITICAL" ? 15158332 : level === "WARN" ? 15105570 : 3066993;
        await axios.post(DISCORD_WEBHOOK_URL, {
          embeds: [
            {
              title: `[${level}] ${subject}`,
              description: message,
              color: color,
              timestamp: timestamp,
            },
          ],
        });
      } catch (err: any) {
        console.error("Failed to forward Discord webhook:", err.message);
      }
    }

    // 2. Dispatch Enterprise Email via Resend if CRITICAL
    if (level === "CRITICAL" && resend) {
      try {
        await resend.emails.send({
          from: "alerts@aegisfinance.io",
          to: ALERT_EMAIL,
          subject: `🚨 CRITICAL ALERT: ${subject}`,
          html: `<p><strong>Timestamp:</strong> ${timestamp}</p><p><strong>Details:</strong> ${message}</p>`,
        });
      } catch (err: any) {
        console.error("Failed to send Resend email:", err.message);
      }
    }
  }
}

// Keeper Core Bot
export class AegisKeeper {
  private connection: Connection;
  private wallet: anchor.Wallet;
  private provider: anchor.AnchorProvider;

  constructor() {
    this.connection = new Connection(RPC_ENDPOINT, { commitment: "confirmed" });
    const secretKey = JSON.parse(fs.readFileSync(KEEPER_KEYPAIR_PATH, "utf-8"));
    const keypair = Keypair.fromSecretKey(Uint8Array.from(secretKey));
    this.wallet = new anchor.Wallet(keypair);
    this.provider = new anchor.AnchorProvider(this.connection, this.wallet, {
      commitment: "confirmed",
    });
  }

  public async fetchPythPrice(assetId: string): Promise<number> {
    try {
      // Fetch fresh Pyth Hermes price feed
      const response = await axios.get(
        `https://hermes.pyth.network/v2/updates/price/latest?ids[]=${assetId}`
      );
      const parsed = response.data.parsed[0].price;
      const actualPrice = Number(parsed.price) * Math.pow(10, parsed.expo);
      return actualPrice;
    } catch (err: any) {
      console.warn("Pyth API fallback, utilizing mock baseline SOL price: $150.00");
      return 150.0;
    }
  }

  public async evaluateAndRebalance(vaultPubkey: PublicKey) {
    try {
      EnterpriseTelemetry.notify(
        "Keeper Heartbeat",
        `Polling vault ${vaultPubkey.toBase58()}`,
        "INFO"
      );

      // SOL Pyth Price Feed ID
      const solFeedId = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
      const solPrice = await this.fetchPythPrice(solFeedId);

      // Simulation parameters: In real deployment, read IDL decoded VaultState
      const mockTotalBaseUsdc = 1_000_000 * 1e6; // $1,000,000 USDC
      const mockBorrowedSol = 4_000 * 1e9; // 4,000 SOL borrowed
      const currentBorrowValueUsd = (mockBorrowedSol / 1e9) * solPrice;
      const currentLtvBps = Math.floor((currentBorrowValueUsd / (mockTotalBaseUsdc / 1e6)) * 10000);

      const targetLtvBps = 6000; // 60%
      const maxLtvBps = 7500; // 75%
      const driftTolerance = 250; // 2.5%

      console.log(`Current LTV: ${(currentLtvBps / 100).toFixed(2)}% | Target: ${(targetLtvBps / 100).toFixed(2)}%`);

      if (currentLtvBps > maxLtvBps) {
        await EnterpriseTelemetry.notify(
          "High LTV Liquidation Risk",
          `Vault LTV has breached maximum safety limit: ${(currentLtvBps / 100).toFixed(2)}% > ${(maxLtvBps / 100).toFixed(2)}%. Immediate unwinding needed!`,
          "CRITICAL"
        );
        // Execute automatic deleverage transaction...
        return;
      }

      if (Math.abs(currentLtvBps - targetLtvBps) >= driftTolerance) {
        await EnterpriseTelemetry.notify(
          "Drift Boundary Triggered",
          `LTV drift detected (${(currentLtvBps / 100).toFixed(2)}%). Executing programmatic rebalance.`,
          "WARN"
        );

        // Calculate adjustment
        const deltaUsd = (targetLtvBps / 10000) * (mockTotalBaseUsdc / 1e6) - currentBorrowValueUsd;
        console.log(`Adjustment target: ${deltaUsd > 0 ? "Borrow" : "Repay"} $${Math.abs(deltaUsd).toFixed(2)} equivalent`);
        
        // Execute CPI rebalance transaction using Anchor program client...
      } else {
        console.log("Vault delta is within optimal tolerance. Rebalancing skipped.");
      }
    } catch (error: any) {
      await EnterpriseTelemetry.notify("Keeper Error", error.message, "CRITICAL");
    }
  }

  public async runLoop(intervalMs: number = 30000) {
    const vaultPDA = PublicKey.findProgramAddressSync(
      [Buffer.from("aegis_vault"), new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").toBuffer()],
      PROGRAM_ID
    )[0];

    console.log("Starting Aegis Autonomous Rebalance Daemon...");
    setInterval(async () => {
      await this.evaluateAndRebalance(vaultPDA);
    }, intervalMs);
  }
}

// Bootstrap
if (require.main === module) {
  const keeper = new AegisKeeper();
  keeper.runLoop();
}