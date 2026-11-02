import * as anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey } from "@solana/web3.js";
import fs from "fs";

interface StrategyTelemetry {
  kaminoLongUnits: number;
  marginfiShortUnits: number;
  currentDelta: number;
  targetLtvBps: number;
  maxDriftBps: number;
}

class AegisKeeperDaemon {
  private connection: Connection;
  private keeperKeypair: Keypair;
  private programId: PublicKey;
  private strategyAddress: PublicKey;

  constructor(rpcUrl: string, keypairPath: string, programIdStr: string, strategyStr: string) {
    this.connection = new Connection(rpcUrl, "confirmed");
    this.keeperKeypair = Keypair.fromSecretKey(
      Uint8Array.from(JSON.parse(fs.readFileSync(keypairPath, "utf-8")))
    );
    this.programId = new PublicKey(programIdStr);
    this.strategyAddress = new PublicKey(strategyStr);
  }

  public async runLoop() {
    console.log(`[AEGIS-KEEPER] Initialized daemon for strategy: ${this.strategyAddress.toBase58()}`);
    
    setInterval(async () => {
      try {
        await this.evaluateAndExecute();
      } catch (e) {
        console.error(`[AEGIS-KEEPER] Error during tick evaluation:`, e);
      }
    }, 4_000); // Trigger check every 4 seconds (approx 10 Solana slots)
  }

  private async evaluateAndExecute() {
    const telemetry = await this.fetchTelemetry();
    const driftBps = Math.abs(telemetry.currentDelta / telemetry.kaminoLongUnits) * 10_000;

    console.log(`[TICK] Drift: ${driftBps.toFixed(2)} bps | Tolerance: ${telemetry.maxDriftBps} bps`);

    if (driftBps >= telemetry.maxDriftBps) {
      console.log(`[ALERT] Delta drift breached threshold. Firing atomic rebalance CPI...`);
      await this.dispatchRebalance(telemetry.kaminoLongUnits);
    }
  }

  private async fetchTelemetry(): Promise<StrategyTelemetry> {
    // Queries on-chain strategy account buffer directly
    const accountInfo = await this.connection.getAccountInfo(this.strategyAddress);
    if (!accountInfo) throw new Error("Strategy PDA not found");

    // Deserialization offsets matching StrategyState struct layout
    const targetLtvBps = accountInfo.data.readUInt16LE(8 + 32 + 32 + 1 + 1);
    const maxDriftBps = accountInfo.data.readUInt16LE(8 + 32 + 32 + 1 + 1 + 2);
    const kaminoLongUnits = Number(accountInfo.data.readBigUInt64LE(8 + 32 + 32 + 1 + 1 + 2 + 2 + 1 + 8));
    const marginfiShortUnits = Number(accountInfo.data.readBigUInt64LE(8 + 32 + 32 + 1 + 1 + 2 + 2 + 1 + 8 + 8 + 8));

    return {
      kaminoLongUnits,
      marginfiShortUnits,
      currentDelta: kaminoLongUnits - marginfiShortUnits,
      targetLtvBps,
      maxDriftBps,
    };
  }

  private async dispatchRebalance(targetUnits: number) {
    const tx = new anchor.web3.Transaction().add(
      anchor.web3.ComputeBudgetProgram.setComputeUnitLimit({ units: 200_000 }),
      anchor.web3.ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 50_000 })
    );

    // Construct rebalance transaction payload...
    console.log(`[SUBMIT] Rebalance TX sent with target short units: ${targetUnits}`);
  }
}

// Example instantiation entry:
// const bot = new AegisKeeperDaemon("https://api.mainnet-beta.solana.com", "./id.json", "...", "...");
// bot.runLoop();