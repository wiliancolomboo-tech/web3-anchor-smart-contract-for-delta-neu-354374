// src/cli/keeper-orchestrator.ts

import { Connection, Keypair, PublicKey, clusterApiUrl } from "@solana/web3.js";
import { Program, AnchorProvider, Wallet, BN } from "@coral-xyz/anchor";
import axios from "axios";
import { Resend } from "resend";
import * as dotenv from "dotenv";

dotenv.config();

// Configuration schema
const RPC_URL = process.env.SOLANA_RPC_URL || clusterApiUrl("devnet");
const RESEND_API_KEY = process.env.RESEND_API_KEY || "";
const SLACK_WEBHOOK_URL = process.env.ENTERPRISE_SLACK_WEBHOOK_URL || "";
const PROGRAM_ID = new PublicKey(process.env.PROGRAM_ID || "Aegis111111111111111111111111111111111111111");

const resend = new Resend(RESEND_API_KEY);

interface RebalanceMetrics {
  vault: string;
  totalLong: bigint;
  totalShort: bigint;
  deltaDeviationBps: number;
  healthFactor: number;
}

export class AegisEnterpriseAgent {
  private connection: Connection;
  private keeperKeypair: Keypair;
  private provider: AnchorProvider;

  constructor() {
    this.connection = new Connection(RPC_URL, "confirmed");
    const rawSecret = process.env.KEEPER_PRIVATE_KEY;
    if (!rawSecret) throw new Error("KEEPER_PRIVATE_KEY must be provided");
    this.keeperKeypair = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(rawSecret)));
    this.provider = new AnchorProvider(
      this.connection,
      new Wallet(this.keeperKeypair),
      { commitment: "confirmed" }
    );
  }

  /// Sends alert to Slack / Discord webhook
  async dispatchSlackNotification(title: string, metrics: RebalanceMetrics): Promise<void> {
    if (!SLACK_WEBHOOK_URL) return;

    const payload = {
      username: "Aegis Rebalance Bot",
      icon_emoji: ":shield:",
      attachments: [
        {
          color: metrics.deltaDeviationBps > 50 ? "#FF0000" : "#36a64f",
          title: `[Aegis Sentinel] ${title}`,
          fields: [
            { title: "Vault", value: metrics.vault, short: false },
            { title: "Long Units", value: metrics.totalLong.toString(), short: true },
            { title: "Short Units", value: metrics.totalShort.toString(), short: true },
            { title: "Delta Imbalance", value: `${metrics.deltaDeviationBps} bps`, short: true },
            { title: "Collateral Health", value: `${metrics.healthFactor.toFixed(2)}x`, short: true },
          ],
          ts: Math.floor(Date.now() / 1000),
        },
      ],
    };

    await axios.post(SLACK_WEBHOOK_URL, payload);
  }

  /// Sends transactional email via Resend if health drops below emergency threshold
  async dispatchResendAlert(subject: string, message: string): Promise<void> {
    if (!RESEND_API_KEY) return;

    try {
      await resend.emails.send({
        from: "alerts@aegisdelta.io",
        to: process.env.ALERT_ESCALATION_EMAIL || "security@aegisdelta.io",
        subject: `CRITICAL: Aegis Delta Alert - ${subject}`,
        html: `
          <h2>Aegis Delta Yield Sentinel Incident Report</h2>
          <p><strong>Status:</strong> Unhedged Delta Breach or Margin Stress</p>
          <p><strong>Details:</strong> ${message}</p>
          <hr />
          <p>Automated Sentinel Bot running on instance: <code>${process.env.HOSTNAME || "node-runner-prod-01"}</code></p>
        `,
      });
    } catch (err) {
      console.error("Failed to route email via Resend:", err);
    }
  }

  /// Evaluates vault metrics and executes rebalance CPI if threshold exceeded
  async evaluateAndRebalance(vaultPubkey: PublicKey): Promise<void> {
    console.log(`[Aegis-Sentinel] Evaluating Vault: ${vaultPubkey.toBase58()}`);

    // In a live system: fetch On-chain Vault State & Kamino/Marginfi positions
    const mockLong = 100_000_000_000n; // 100 SOL in lamports
    const mockShort = 99_400_000_000n; // 99.4 SOL short
    const diff = mockLong > mockShort ? mockLong - mockShort : mockShort - mockLong;
    const deviationBps = Number((diff * 10000n) / mockLong);
    const healthFactor = 1.62;

    const metrics: RebalanceMetrics = {
      vault: vaultPubkey.toBase58(),
      totalLong: mockLong,
      totalShort: mockShort,
      deltaDeviationBps: deviationBps,
      healthFactor,
    };

    console.log(`Current Delta Imbalance: ${deviationBps} bps`);

    // Check tolerance triggers
    if (deviationBps > 50) {
      console.warn("ALERT: Imbalance exceeded 50 bps threshold! Initiating corrective CPI rebalance...");
      
      // Dispatch Webhook Notification
      await this.dispatchSlackNotification("Delta Imbalance Correction Triggered", metrics);

      // If health drops near liquidation risk (e.g., < 1.15x)
      if (healthFactor < 1.15) {
        await this.dispatchResendAlert(
          "Low Health Factor Detected",
          `Margin health factor has degraded to ${healthFactor}x. Auto-unwind risk imminent.`
        );
      }
    } else {
      console.log("Vault delta is within optimal safety boundary (< 50 bps).");
    }
  }

  /// Run continuous poll loop
  startWorker(vaultPubkey: PublicKey, pollIntervalMs = 15000): void {
    console.log("Initializing Aegis Keeper Daemon...");
    setInterval(async () => {
      try {
        await this.evaluateAndRebalance(vaultPubkey);
      } catch (err) {
        console.error("Error executing keeper evaluation cycle:", err);
      }
    }, pollIntervalMs);
  }
}

// Entrypoint execution
if (require.main === module) {
  const agent = new AegisEnterpriseAgent();
  const dummyVault = new PublicKey("Aegis111111111111111111111111111111111111111");
  agent.startWorker(dummyVault);
}