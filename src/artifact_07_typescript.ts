import * as anchor from "@coral-xyz/anchor";
import { Program, Wallet } from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey, ComputeBudgetProgram } from "@solana/web3.js";
import axios from "axios";
import * as fs from "fs";
import * as dotenv from "dotenv";

dotenv.config();

// Enterprise Configurations
const RPC_URL = process.env.SOLANA_RPC_URL || "http://127.0.0.1:8899";
const KEEPER_KEYPAIR_PATH = process.env.KEEPER_KEYPAIR_PATH || "./keeper-keypair.json";
const PROGRAM_ID = new PublicKey(process.env.PROGRAM_ID || "AuraDe1taYieldRouter1111111111111111111111");
const USDC_MINT = new PublicKey(process.env.USDC_MINT || "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const SLACK_WEBHOOK_URL = process.env.SLACK_WEBHOOK_URL || "";
const RESEND_API_KEY = process.env.RESEND_API_KEY || "";
const ESCALATION_EMAIL = process.env.ESCALATION_EMAIL || "ops@institution.fi";

// Tolerances
const REBALANCE_INTERVAL_MS = 15_000;
const MAX_DELTA_DRIFT_LAMPORTS = 50_000_000; // 0.05 SOL

interface TelemetryReport {
  timestamp: string;
  signature?: string;
  spotSol: number;
  borrowSol: number;
  deltaDiff: number;
  status: "OK" | "WARNING" | "CRITICAL";
  message: string;
}

export class DeltaNeutralKeeper {
  private connection: Connection;
  private wallet: Wallet;
  private provider: anchor.AnchorProvider;
  private program: Program;
  private vaultPda!: PublicKey;

  constructor() {
    this.connection = new Connection(RPC_URL, { commitment: "confirmed" });
    const secretKey = JSON.parse(fs.readFileSync(KEEPER_KEYPAIR_PATH, "utf-8"));
    const keypair = Keypair.fromSecretKey(Uint8Array.from(secretKey));
    this.wallet = new Wallet(keypair);

    this.provider = new anchor.AnchorProvider(this.connection, this.wallet, {
      preflightCommitment: "confirmed",
    });
    anchor.setProvider(this.provider);

    // Dynamic program loading via IDL or direct client instantiation
    const idl = JSON.parse(fs.readFileSync("./target/idl/aura_delta_router.json", "utf-8"));
    this.program = new Program(idl, this.provider);
  }

  public async init() {
    [this.vaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault_state"), USDC_MINT.toBuffer()],
      PROGRAM_ID
    );
    console.log(`[INIT] Keeper Online: ${this.wallet.publicKey.toBase58()}`);
    console.log(`[INIT] Target Vault PDA: ${this.vaultPda.toBase58()}`);
  }

  public async checkAndRebalance() {
    try {
      const vaultAccount: any = await this.program.account.vaultState.fetch(this.vaultPda);

      const spotSol = vaultAccount.spotSolDeposited.toNumber();
      const borrowSol = vaultAccount.hedgedSolBorrowed.toNumber();
      const delta = Math.abs(spotSol - borrowSol);

      console.log(`[MONITOR] Spot SOL: ${spotSol / 1e9} | Borrow SOL: ${borrowSol / 1e9} | Delta: ${delta / 1e9}`);

      if (delta > MAX_DELTA_DRIFT_LAMPORTS) {
        console.warn(`[ACTION REQUIRED] Delta Drift exceeded threshold: ${delta / 1e9} SOL. Dispatching Rebalance...`);
        
        // Target 50/50 exact hedge alignment
        const targetAllocation = Math.floor((spotSol + borrowSol) / 2);
        const oraclePriceScaled = 150_000_000; // Simulated $150.00 Pyth price

        // Priority fee instruction to ensure inclusion
        const modifyComputeUnits = ComputeBudgetProgram.setComputeUnitLimit({ units: 400_000 });
        const addPriorityFee = ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 50_000 });

        const tx = await this.program.methods
          .rebalance(
            new anchor.BN(oraclePriceScaled),
            new anchor.BN(targetAllocation),
            new anchor.BN(targetAllocation)
          )
          .accounts({
            vault: this.vaultPda,
            vaultSigner: this.vaultPda,
            keeper: this.wallet.publicKey,
            marginfiProgram: Keypair.generate().publicKey, // Substitute with live Marginfi v2 ID
            marginfiAccount: Keypair.generate().publicKey,
            kaminoProgram: Keypair.generate().publicKey,   // Substitute with live Kamino Strategy ID
            kaminoStrategy: Keypair.generate().publicKey,
          })
          .preInstructions([modifyComputeUnits, addPriorityFee])
          .rpc();

        const report: TelemetryReport = {
          timestamp: new Date().toISOString(),
          signature: tx,
          spotSol: targetAllocation,
          borrowSol: targetAllocation,
          deltaDiff: 0,
          status: "OK",
          message: `Vault successfully rebalanced. Position delta neutralized.`,
        };

        await this.notifySlack(report);
      }
    } catch (err: any) {
      console.error(`[ERROR] Keeper execution failed:`, err);
      await this.notifyEmergencyAlert(err.message || String(err));
    }
  }

  private async notifySlack(report: TelemetryReport) {
    if (!SLACK_WEBHOOK_URL) return;
    try {
      await axios.post(SLACK_WEBHOOK_URL, {
        text: `⚡ *Aura-Delta Rebalance Event Executed*`,
        attachments: [
          {
            color: "#36a64f",
            fields: [
              { title: "Tx Signature", value: report.signature, short: false },
              { title: "Status", value: report.status, short: true },
              { title: "Net Drift After Tx", value: `${report.deltaDiff} SOL`, short: true },
            ],
            footer: "Aura-Delta Autonomous Keeper",
            ts: Math.floor(Date.now() / 1000),
          },
        ],
      });
    } catch (netErr) {
      console.error("Slack alert dispatch failed", netErr);
    }
  }

  private async notifyEmergencyAlert(errorMsg: string) {
    if (!RESEND_API_KEY) return;
    try {
      await axios.post(
        "https://api.resend.com/emails",
        {
          from: "alerts@auradelta.fi",
          to: [ESCALATION_EMAIL],
          subject: "CRITICAL: Aura-Delta Yield Router Execution Failure",
          html: `<h3>Emergency Incident Triggered</h3>
                 <p><strong>Time:</strong> ${new Date().toISOString()}</p>
                 <p><strong>Error Message:</strong> ${errorMsg}</p>
                 <p>Keeper execution halted. Investigate RPC status or liquidation buffer immediately.</p>`,
        },
        {
          headers: {
            Authorization: `Bearer ${RESEND_API_KEY}`,
            "Content-Type": "application/json",
          },
        }
      );
    } catch (emailErr) {
      console.error("Resend API emergency notification failed", emailErr);
    }
  }

  public run() {
    setInterval(() => this.checkAndRebalance(), REBALANCE_INTERVAL_MS);
  }
}

// Execution Entrypoint
(async () => {
  const keeper = new DeltaNeutralKeeper();
  await keeper.init();
  keeper.run();
})();