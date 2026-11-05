import axios from 'axios';
import { Resend } from 'resend';

export interface RebalanceLog {
  txSignature: string;
  kaminoAllocated: number;
  marginfiHedge: number;
  yieldSpreadBps: number;
  netApyEstimate: number;
  healthRatio: number;
}

export class EnterpriseTelemetry {
  private resendClient: Resend | null = null;
  private discordWebhookUrl: string | null = null;
  private slackWebhookUrl: string | null = null;
  private alertEmail: string | null = null;

  constructor() {
    if (process.env.RESEND_API_KEY) {
      this.resendClient = new Resend(process.env.RESEND_API_KEY);
    }
    this.discordWebhookUrl = process.env.DISCORD_WEBHOOK_URL || null;
    this.slackWebhookUrl = process.env.SLACK_WEBHOOK_URL || null;
    this.alertEmail = process.env.ALERT_RECIPIENT_EMAIL || null;
  }

  public async notifyRebalance(log: RebalanceLog): Promise<void> {
    const timestamp = new Date().toISOString();
    const formattedPayload = {
      title: "🚨 Delta-Neutral Yield Rebalance Executed",
      description: `Atomic CPI rebalance completed across Kamino & Marginfi.`,
      fields: [
        { name: "Transaction Signature", value: `[${log.txSignature}](https://solscan.io/tx/${log.txSignature})` },
        { name: "Kamino Supply Leg", value: `$${log.kaminoAllocated.toLocaleString()}`, inline: true },
        { name: "Marginfi Borrow Hedge", value: `$${log.marginfiHedge.toLocaleString()}`, inline: true },
        { name: "Spread Captured", value: `+${(log.yieldSpreadBps / 100).toFixed(2)}%`, inline: true },
        { name: "Estimated Net APY", value: `${log.netApyEstimate.toFixed(2)}%`, inline: true },
        { name: "Health Factor", value: `${log.healthRatio.toFixed(3)}`, inline: true },
      ],
      timestamp,
    };

    // 1. Dispatch Discord Alert
    if (this.discordWebhookUrl) {
      try {
        await axios.post(this.discordWebhookUrl, { embeds: [formattedPayload] });
      } catch (err) {
        console.error("Failed to dispatch Discord webhook:", err);
      }
    }

    // 2. Dispatch Slack Webhook
    if (this.slackWebhookUrl) {
      try {
        await axios.post(this.slackWebhookUrl, {
          text: `*Delta-Neutral Router Event*\nTx: https://solscan.io/tx/${log.txSignature}\nKamino: $${log.kaminoAllocated} | Marginfi Hedge: $${log.marginfiHedge} | Spread: ${(log.yieldSpreadBps / 100).toFixed(2)}%`,
        });
      } catch (err) {
        console.error("Failed to dispatch Slack webhook:", err);
      }
    }

    // 3. Dispatch Resend Enterprise Audit Email
    if (this.resendClient && this.alertEmail) {
      try {
        await this.resendClient.emails.send({
          from: 'router-telemetry@enterprise.fi',
          to: this.alertEmail,
          subject: `[Audit] Rebalance Executed - Tx ${log.txSignature.slice(0, 8)}...`,
          html: `
            <h2>Delta-Neutral Strategy Rebalance Confirmation</h2>
            <p><strong>Tx Hash:</strong> <a href="https://solscan.io/tx/${log.txSignature}">${log.txSignature}</a></p>
            <ul>
              <li><strong>Kamino Liquidity Leg:</strong> $${log.kaminoAllocated}</li>
              <li><strong>Marginfi Hedge Leg:</strong> $${log.marginfiHedge}</li>
              <li><strong>Spread Captured:</strong> ${(log.yieldSpreadBps / 100).toFixed(2)}%</li>
              <li><strong>Net APY:</strong> ${log.netApyEstimate.toFixed(2)}%</li>
              <li><strong>Vault Health Buffer:</strong> ${log.healthRatio}</li>
            </ul>
            <p>Execution timestamp: ${timestamp}</p>
          `,
        });
      } catch (err) {
        console.error("Failed to send Resend audit email:", err);
      }
    }
  }
}