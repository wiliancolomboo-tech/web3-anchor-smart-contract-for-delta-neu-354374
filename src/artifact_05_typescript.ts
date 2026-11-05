import * as anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey } from "@solana/web3.js";
import axios from "axios";
import * as dotenv from "dotenv";
import * as fs from "fs";

dotenv.config();

interface VaultStateAccount {
  admin: PublicKey;
  depositMint: PublicKey;
  hedgedMint: PublicKey;
  kaminoReserve: PublicKey;
  marginfiBank: PublicKey;
  pythPriceFeed: PublicKey;
  targetLeverageBps: anchor.BN;
  rebalanceThresholdBps: anchor.BN;
  totalUsdcDeposited: anchor.BN;
  totalHedgedBorrowed: anchor.BN;
  isFrozen: boolean;
}

class DeltaNeutralSentinel {
  private connection: Connection;
  private wallet: anchor.Wallet;
  private program: anchor.Program;
  private vaultPubkey: PublicKey;
  private discordWebhookUrl: string;

  constructor() {
    const rpcUrl = process.env.SOLANA_RPC_URL || "https://api.mainnet-beta.solana.com";
    this.connection = new Connection(rpcUrl, "confirmed");

    const keypairData = JSON.parse(
      fs.readFileSync(process.env.KEEPER_KEYPAIR_PATH || "./keeper-wallet.json", "utf-8")
    );
    const keeperKeypair = Keypair.fromSecretKey(new Uint8Array(keypairData));
    this.wallet = new anchor.Wallet(keeperKeypair);

    const provider = new anchor.AnchorProvider(this.connection, this.wallet, {
      commitment: "confirmed",
    });
    anchor.setProvider(provider);

    const idl = JSON.parse(fs.readFileSync("./target/idl/delta_neutral_router.json", "utf8"));
    const programId = new PublicKey(process.env.ROUTER_PROGRAM_ID!);
    this.program = new anchor.Program(idl, programId, provider);

    this.vaultPubkey = new PublicKey(process.env.VAULT_STATE_PUBKEY!);
    this.discordWebhookUrl = process.env.DISCORD_WEBHOOK_URL || "";
  }

  public async startLoop(pollIntervalMs = 15000): Promise<void> {
    console.log(`[SENTINEL] Active. Monitoring Vault: ${this.vaultPubkey.toBase58()}`);
    while (true) {
      try {
        await this.evaluateVaultHealth();
      } catch (err: any) {
        console.error(`[SENTINEL ERROR] Loop exception:`, err.message);
        await this.sendAlert("Vault Evaluation Failure", `Exception: ${err.message}`, 0xff0000);
      }
      await new Promise((resolve) => setTimeout(resolve, pollIntervalMs));
    }
  }

  private async evaluateVaultHealth(): Promise<void> {
    const vault = (await this.program.account.vaultState.fetch(
      this.vaultPubkey
    )) as VaultStateAccount;

    if (vault.isFrozen) {
      console.warn("[SENTINEL] Vault is paused by admin. Skipping rebalance check.");
      return;
    }

    const depositedUsdc = vault.totalUsdcDeposited.toNumber() / 1e6;
    const currentHedgedSol = vault.totalHedgedBorrowed.toNumber() / 1e9;

    console.log(
      `[SNAPSHOT] Deposited: $${depositedUsdc.toFixed(2)} USDC | Hedged Borrow: ${currentHedgedSol.toFixed(4)} SOL`
    );

    // Call Rebalance Instruction (Contract independently validates oracle and BPS thresholds)
    try {
      const tx = await this.program.methods
        .rebalanceVault()
        .accounts({
          keeper: this.wallet.publicKey,
          vaultState: this.vaultPubkey,
          vaultAuthority: PublicKey.findProgramAddressSync(
            [Buffer.from("vault_authority"), this.vaultPubkey.toBuffer()],
            this.program.programId
          )[0],
          vaultHedgedAta: process.env.VAULT_HEDGED_ATA!,
          pythPriceFeed: vault.pythPriceFeed,
          marginfiProgram: new PublicKey("MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA"),
          marginfiBank: vault.marginfiBank,
          marginfiAccount: process.env.MARGINFI_ACCOUNT_PUBKEY!,
          tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        })
        .rpc();

      console.log(`[REBALANCE EXECUTED] Tx: ${tx}`);
      await this.sendAlert(
        "Delta Neutral Rebalance Success",
        `Deviation exceeded safe threshold. Successfully re-indexed delta balance.\n**Tx Signature**: \`${tx}\``,
        0x00ff00
      );
    } catch (err: any) {
      if (err.message.includes("RebalanceNotNeeded")) {
        // Healthy state; delta within bounds
        return;
      }
      throw err;
    }
  }

  private async sendAlert(title: string, description: string, color: number): Promise<void> {
    if (!this.discordWebhookUrl) return;
    try {
      await axios.post(this.discordWebhookUrl, {
        embeds: [
          {
            title: `[Solana Delta-Router] ${title}`,
            description,
            color,
            timestamp: new Date().toISOString(),
          },
        ],
      });
    } catch (e: any) {
      console.error("[ALERT DISPATCH FAIL]", e.message);
    }
  }
}

// Entrypoint
if (require.main === module) {
  const sentinel = new DeltaNeutralSentinel();
  sentinel.startLoop();
}