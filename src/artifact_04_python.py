import asyncio
import os
import logging
from dataclasses import dataclass
from solana.rpc.async_api import AsyncClient
from solders.keypair import Keypair
from solders.pubkey import Pubkey
from solders.instruction import Instruction, AccountMeta
from solders.transaction import VersionedTransaction
from solders.message import MessageV0
import struct

logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")

PROGRAM_ID = Pubkey.from_string("DNYR111111111111111111111111111111111111111")
USDC_MINT = Pubkey.from_string("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
SOL_PRICE_FEED = Pubkey.from_string("H6ARHf6YXhGYeQfUzQNGk6rDNnLBQKrenN712K4AQJEG")

@dataclass
class VaultState:
    vault_id: int
    target_ltv_bps: int
    rebalance_threshold_bps: int
    total_spot_long: int
    total_borrow_short: int
    is_frozen: bool

class DnyrKeeperBot:
    def __init__(self, rpc_url: str, keeper_keypair_path: str):
        self.client = AsyncClient(rpc_url)
        with open(os.path.expanduser(keeper_keypair_path), "r") as f:
            secret = list(map(int, f.read().strip("[] \n").split(",")))
            self.keeper = Keypair.from_bytes(bytes(secret[:64]))
        logging.info(f"Keeper initialized with address: {self.keeper.pubkey()}")

    async def get_vault_data(self, vault_pda: Pubkey) -> VaultState:
        resp = await self.client.get_account_info(vault_pda)
        data = resp.value.data
        # Skip 8-byte Anchor discriminator
        parsed = struct.unpack("<QQ32sBB32s32sQQQQbQ", data[8:8+146])
        return VaultState(
            vault_id=parsed[0],
            target_ltv_bps=parsed[6],
            rebalance_threshold_bps=parsed[7],
            total_spot_long=parsed[8],
            total_borrow_short=parsed[9],
            is_frozen=bool(parsed[10]),
        )

    def calculate_drift_bps(self, spot_long: int, short_borrow: int) -> float:
        if spot_long == 0:
            return 0.0
        drift = abs(spot_long - short_borrow)
        return (drift / spot_long) * 10_000

    async def run(self, vault_id: int):
        vault_id_bytes = struct.pack("<Q", vault_id)
        vault_pda, _ = Pubkey.find_program_address([b"vault", vault_id_bytes], PROGRAM_ID)
        vault_auth, _ = Pubkey.find_program_address([b"authority", vault_id_bytes], PROGRAM_ID)
        collateral_escrow, _ = Pubkey.find_program_address([b"collateral", vault_id_bytes], PROGRAM_ID)

        logging.info(f"Monitoring Vault PDA: {vault_pda}")

        while True:
            try:
                state = await self.get_vault_data(vault_pda)
                if state.is_frozen:
                    logging.warning("Vault is currently frozen. Skipping monitoring cycle.")
                    await asyncio.sleep(10)
                    continue

                drift_bps = self.calculate_drift_bps(state.total_spot_long, state.total_borrow_short)
                logging.info(f"Drift: {drift_bps:.2f} BPS | Threshold: {state.rebalance_threshold_bps} BPS")

                if drift_bps >= state.rebalance_threshold_bps:
                    logging.warning(f"Delta drift breach detected! Executing rebalance transaction...")
                    
                    # 0 = Adjust Spot Long, 1 = Adjust Borrow Short
                    action = 1 if state.total_spot_long > state.total_borrow_short else 0
                    diff_amount = abs(state.total_spot_long - state.total_borrow_short)

                    # Rebalance Instruction Discriminator
                    rebalance_disc = bytes([0x9d, 0x4f, 0x12, 0x33, 0x11, 0x90, 0x77, 0xbc])
                    ix_data = rebalance_disc + struct.pack("<BQ", action, diff_amount)

                    # Mock account resolution for Keeper Reward ATA
                    keeper_ata, _ = Pubkey.find_program_address(
                        [bytes(self.keeper.pubkey()), bytes(Pubkey.from_string("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")), bytes(USDC_MINT)],
                        Pubkey.from_string("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")
                    )

                    ix = Instruction(
                        PROGRAM_ID,
                        ix_data,
                        [
                            AccountMeta(vault_pda, is_signer=False, is_writable=True),
                            AccountMeta(vault_auth, is_signer=False, is_writable=True),
                            AccountMeta(collateral_escrow, is_signer=False, is_writable=True),
                            AccountMeta(keeper_ata, is_signer=False, is_writable=True),
                            AccountMeta(self.keeper.pubkey(), is_signer=True, is_writable=True),
                            AccountMeta(SOL_PRICE_FEED, is_signer=False, is_writable=False),
                            AccountMeta(Pubkey.from_string("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"), is_signer=False, is_writable=False),
                            AccountMeta(Pubkey.from_string("SysvarC1ock11111111111111111111111111111111"), is_signer=False, is_writable=False),
                        ]
                    )

                    latest_blockhash = (await self.client.get_latest_blockhash()).value.blockhash
                    msg = MessageV0.try_compile(self.keeper.pubkey(), [ix], [], latest_blockhash)
                    tx = VersionedTransaction(msg, [self.keeper])

                    sig = await self.client.send_transaction(tx)
                    logging.info(f"Rebalance transaction broadcasted with signature: {sig.value}")
                
            except Exception as e:
                logging.error(f"Keeper cycle exception: {str(e)}")

            await asyncio.sleep(2.5)

if __name__ == "__main__":
    bot = DnyrKeeperBot("http://127.0.0.1:8899", "~/.config/solana/id.json")
    asyncio.run(bot.run(vault_id=42))