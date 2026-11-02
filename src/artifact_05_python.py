"""
Off-chain Automated Keeper Daemon
Monitors Marginfi vs. Kamino position deltas and dispatches on-chain rebalancing
transactions whenever systemic drift surpasses 150 basis points.
"""

import asyncio
import logging
from dataclasses import dataclass
from solana.rpc.async_api import AsyncClient
from solders.pubkey import Pubkey
from solders.keypair import Keypair
from solders.instruction import Instruction, AccountMeta
import numpy as np

logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")

MAX_ALLOWED_DELTA_DRIFT_BPS = 150 # 1.5% Drift Tolerance
CHECK_INTERVAL_SECONDS = 5

@dataclass
class PositionTelemetry:
    borrowed_sol: float
    hedged_sol: float
    health_factor: float
    current_sol_price: float

    @property
    def net_delta(self) -> float:
        return self.hedged_sol - self.borrowed_sol

    @property
    def drift_bps(self) -> float:
        baseline = max(self.borrowed_sol, 1e-6)
        return (abs(self.net_delta) / baseline) * 10_000

class DeltaNeutralKeeper:
    def __init__(self, rpc_url: str, keeper_keypair: Keypair, program_id: Pubkey, vault_pda: Pubkey):
        self.client = AsyncClient(rpc_url)
        self.keeper = keeper_keypair
        self.program_id = program_id
        self.vault_pda = vault_pda

    async def fetch_telemetry(self) -> PositionTelemetry:
        """
        Polls Pyth, Marginfi, and Kamino accounts to reconstruct active risk metrics.
        """
        account_info = await self.client.get_account_info(self.vault_pda)
        if not account_info.value:
            raise ValueError("Failed to locate Vault Account data on-chain.")

        # Binary parse of VaultState: offset total_sol_borrowed = 136, total_sol_hedged = 144
        data = account_info.value.data
        borrowed_lamports = int.from_bytes(data[136:144], "little")
        hedged_lamports = int.from_bytes(data[144:152], "little")

        borrowed_sol = borrowed_lamports / 1e9
        hedged_sol = hedged_lamports / 1e9
        
        # Mock values for oracle and health factor (would fetch live in production)
        return PositionTelemetry(
            borrowed_sol=borrowed_sol,
            hedged_sol=hedged_sol,
            health_factor=1.42,
            current_sol_price=175.50
        )

    async def dispatch_rebalance(self, telemetry: PositionTelemetry):
        logging.warning(
            f"DRIFT DETECTED: Delta Drift = {telemetry.drift_bps:.2f} BPS "
            f"(Net Delta: {telemetry.net_delta:.4f} SOL). Invoking rebalance_delta CPI..."
        )
        is_increasing_long = telemetry.net_delta < 0
        adjust_amount_lamports = int(abs(telemetry.net_delta) * 1e9)

        # Build instruction data
        # Instruction Discriminator: [8 bytes] + adjust_amount: [u64, 8 bytes] + is_increasing_long: [bool, 1 byte]
        discriminator = bytes([0x67, 0xe1, 0x05, 0xca, 0x12, 0x88, 0x9f, 0x21])
        payload = discriminator + adjust_amount_lamports.to_bytes(8, "little") + (b'\x01' if is_increasing_long else b'\x00')

        # Dummy account metacontext
        ix = Instruction(
            program_id=self.program_id,
            data=payload,
            accounts=[
                AccountMeta(pubkey=self.keeper.pubkey(), is_signer=True, is_writable=True),
                AccountMeta(pubkey=self.vault_pda, is_signer=False, is_writable=True),
            ]
        )
        logging.info(f"Submitting transaction signed by Keeper {self.keeper.pubkey()}...")
        # Production would invoke client.send_transaction(tx, self.keeper)

    async def run(self):
        logging.info("Keeper loop initialized. Tracking Kamino & Marginfi exposure...")
        while True:
            try:
                telemetry = await self.fetch_telemetry()
                logging.info(
                    f"Status: Borrowed={telemetry.borrowed_sol:.3f} SOL | "
                    f"Hedged={telemetry.hedged_sol:.3f} SOL | Drift={telemetry.drift_bps:.1f} bps"
                )
                if telemetry.drift_bps > MAX_ALLOWED_DELTA_DRIFT_BPS:
                    await self.dispatch_rebalance(telemetry)
            except Exception as e:
                logging.error(f"Keeper runtime error: {str(e)}")
            await asyncio.sleep(CHECK_INTERVAL_SECONDS)

if __name__ == "__main__":
    dummy_key = Keypair()
    dummy_prog = Pubkey.from_string("DeltANeutraL11111111111111111111111111111111")
    dummy_vault = Pubkey.from_string("11111111111111111111111111111111")
    bot = DeltaNeutralKeeper("https://api.devnet.solana.com", dummy_key, dummy_prog, dummy_vault)
    asyncio.run(bot.run())