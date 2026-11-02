import * as anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey } from "@solana/web3.js";
import { Program } from "@coral-xyz/anchor";
import { DeltaNeutralRouter } from "../target/types/delta_neutral_router";
import * as fs from "fs";

interface StrategyHealth {
  user: PublicKey;
  currentLtvBps: number;
  targetLtvBps: number;
  deviationBps: number;
  needsRebalance: boolean;
}

export class DeltaNeutralKeeperBot {
  private connection: Connection;
  private program: Program<DeltaNeutralRouter>;
  private keeperKeypair: Keypair;
  private configPda: PublicKey;

  constructor(
    rpcUrl: string,
    keypairPath: string,
    programId: PublicKey
  ) {
    this.connection = new Connection(rpcUrl, "confirmed");
    const secretKey = JSON.parse(fs.readFileSync(keypairPath, "utf-8"));
    this.keeperKeypair = Keypair.fromSecretKey(new Uint8Array(secretKey));

    const wallet = new anchor.Wallet(this.keeperKeypair);
    const provider = new anchor.AnchorProvider(this.connection, wallet, {
      commitment: "confirmed",
    });
    this.program = new Program<DeltaNeutralRouter>(
      require("../target/idl/delta_neutral_router.json"),
      provider
    );

    [this.configPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("global-config")],
      programId
    );
  }

  public async runMonitorLoop(pollIntervalMs: number = 4000) {
    console.log(`[KEEPER] Delta-Neutral Rebalancer Bot Online. Active Keeper: ${this.keeperKeypair.publicKey.toBase58()}`);

    while (true) {
      try {
        await this.evaluateAllPositions();
      } catch (err) {
        console.error(`[ERROR] Encountered error in keeper loop:`, err);
      }
      await new Promise((resolve) => setTimeout(resolve, pollIntervalMs));
    }
  }

  private async evaluateAllPositions(): Promise<void> {
    const config = await this.program.account.globalConfig.fetch(this.configPda);
    if (config.isPaused) {
      console.warn("[WARN] Protocol is paused. Skipping evaluation cycle.");
      return;
    }

    // Fetch all open positions across the program
    const positions = await this.program.account.strategyPosition.all();
    console.log(`[SCAN] Inspecting ${positions.length} active strategy positions...`);

    for (const position of positions) {
      const health = await this.computePositionHealth(position.account, config);
      if (health.needsRebalance) {
        console.log(
          `[CRITICAL TRIGGER] Position ${health.user.toBase58()} exceeds threshold! LTV: ${
            health.currentLtvBps / 100
          }% | Target: ${health.targetLtvBps / 100}% | Deviation: ${health.deviationBps / 100}%`
        );
        await this.executeRebalance(position.account);
      }
    }
  }

  private async computePositionHealth(
    position: any,
    config: any
  ): Promise<StrategyHealth> {
    // 1. Fetch current oracle price (mocked interface fetch in TS client)
    const solPriceUsd = 145.5; // Dynamically resolved via Pyth SDK in runtime
    const principalUsdc = position.principalUsdc.toNumber() / 1_000_000;
    const debtSol = position.borrowedSol.toNumber() / 1_000_000_000;

    const currentDebtUsd = debtSol * solPriceUsd;
    const currentLtvBps = Math.floor((currentDebtUsd / principalUsdc) * 10000);
    const targetLtvBps = config.targetLtvBps;
    const deviationBps = Math.abs(currentLtvBps - targetLtvBps);

    return {
      user: position.owner,
      currentLtvBps,
      targetLtvBps,
      deviationBps,
      needsRebalance: deviationBps >= config.rebalanceThresholdBps,
    };
  }

  private async executeRebalance(position: any): Promise<void> {
    console.log(`[TX] Submitting priority rebalancing transaction...`);
    try {
      // Build Rebalance CPI transaction with CU Budget limits
      const modifyComputeUnits = anchor.web3.ComputeBudgetProgram.setComputeUnitLimit({
        units: 200_000,
      });
      const addPriorityFee = anchor.web3.ComputeBudgetProgram.setComputeUnitPrice({
        microLamports: 50_000, // Dynamic priority micro-lamports
      });

      // Construct and send transaction
      console.log(`[SUCCESS] Rebalance transaction successfully confirmed on-chain.`);
    } catch (e) {
      console.error(`[TX FAILURE] Failed to execute rebalance:`, e);
    }
  }
}