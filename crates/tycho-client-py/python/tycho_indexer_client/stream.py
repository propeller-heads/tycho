import asyncio
import json
import os
import platform
import shutil

from decimal import Decimal
from logging import getLogger
from pathlib import Path
from subprocess import PIPE, STDOUT
from typing import Any

from pydantic import ValidationError

from .dto import Chain, FeedMessage
from .exception import TychoStreamException

log = getLogger(__name__)


class TychoStream:
    def __init__(
        self,
        tycho_url: str,
        exchanges: list[str],
        blockchain: Chain,
        auth_token: str = None,
        min_tvl: Decimal = None,
        min_tvl_range: tuple[Decimal, Decimal] = None,
        include_state: bool = True,
        logs_directory: str = None,
        tycho_client_path: str = None,
        use_tls: bool = True,
        partial_blocks: bool = False,
        include_tvl: bool = False,
        disable_compression: bool = False,
    ):
        """
        Initializes the TychoStream instance.

        Args:
            tycho_url: The URL of the Tycho indexer server.
            exchanges: A list of exchanges to monitor in the stream.
            blockchain: The blockchain chain you are querying (e.g., Ethereum).
            min_tvl: The minimum total value locked (TVL) for filtering data in the stream. Defaults to None.
            min_tvl_range: A tuple of (removal_threshold, addition_threshold). New components will be added to the data stream
                if their TVL exceeds addition_threshold and will be removed from the stream if it drops below removal_threshold.
                Defaults to None. If both min_tvl and min_tvl_range are given, preference is given to min_tvl.
            include_state: Whether to include protocol states in the stream. Defaults to True.
            logs_directory: Directory to store log files. If not specified, a default directory based on the OS is used.
            tycho_client_path: Path to the Tycho client binary. If not specified, the binary is searched for in the system's PATH.
            use_tls: Whether to use TLS connections with `tycho_url` or not. Defaults to `True`.
            partial_blocks: Whether to receive partial block updates on chains that have them. Defaults to `False`.
            include_tvl: Whether to include TVL estimates in messages. Increases startup latency
                due to additional network requests. Defaults to `False`.
            disable_compression: Whether to disable zstd compression for WebSocket messages.
                Compression is enabled by default. Defaults to `False`.
        """
        self.tycho_url = tycho_url
        self.auth_token = auth_token
        self.min_tvl = min_tvl
        self.min_tvl_range = min_tvl_range
        self.tycho_client = None
        self.exchanges = exchanges
        self._include_state = include_state
        self._blockchain = blockchain
        self._logs_directory = logs_directory or get_default_log_directory()
        self._tycho_client_path = tycho_client_path or find_tycho_client()
        self._use_tls = use_tls
        self._partial_blocks = partial_blocks
        self._include_tvl = include_tvl
        self._disable_compression = disable_compression

    async def start(self):
        """Start the tycho-client Rust binary through subprocess"""
        # stdout=PIPE means that the output is piped directly to this Python process
        # stderr=STDOUT combines the stderr and stdout streams

        cmd = [
            "--log-folder", self._logs_directory,
            "--tycho-url", self.tycho_url,
            "--chain", self._blockchain.value,
        ]

        if self.min_tvl is not None:
            cmd.extend(["--min-tvl", str(self.min_tvl)])
        elif self.min_tvl_range is not None:
            cmd.extend(
                [
                    "--remove-tvl-threshold",
                    str(self.min_tvl_range[0]),
                    "--add-tvl-threshold",
                    str(self.min_tvl_range[1]),
                ]
            )

        if self.auth_token:
            cmd.extend(["--auth-key", self.auth_token])

        if not self._include_state:
            cmd.append("--no-state")

        if not self._use_tls:
            cmd.append("--no-tls")

        if self._partial_blocks:
            cmd.append("--partial-blocks")

        if self._include_tvl:
            cmd.append("--include-tvl")

        if self._disable_compression:
            cmd.append("--disable-compression")

        for exchange in self.exchanges:
            cmd.append("--exchange")
            cmd.append(exchange)

        log.debug(
            f"Starting tycho-client binary at {self._tycho_client_path}. CMD: {cmd}"
        )
        self.tycho_client = await asyncio.create_subprocess_exec(
            self._tycho_client_path,
            *cmd,
            stdout=PIPE,
            stderr=STDOUT,
            limit=2 ** 64,
            env={**os.environ, "NO_COLOR": "true"},
        )

    def __aiter__(self):
        return self

    async def __anext__(self) -> FeedMessage:
        if self.tycho_client.stdout.at_eof():
            raise StopAsyncIteration
        line = await self.tycho_client.stdout.readline()

        try:
            if not line:
                exit_code = await self.tycho_client.wait()
                if exit_code == 0:
                    # Clean exit, handle accordingly, possibly without raising an error
                    log.debug("Tycho client exited cleanly.")
                    raise StopAsyncIteration
                else:
                    line = f"Tycho client failed with exit code: {exit_code}"
                    # Non-zero exit code, handle accordingly, possibly by raising an error
                    raise TychoStreamException(line)

            msg = json.loads(line.decode("utf-8"))
        except (json.JSONDecodeError, TychoStreamException):
            # Read the last 10 lines from the log file available under TYCHO_CLIENT_LOG_FOLDER
            # and raise an exception with the last 10 lines
            error_msg = f"Invalid JSON output on tycho. Original line: {line}."
            with open(Path(self._logs_directory) / "dev_logs.log", "r") as f:
                lines = f.readlines()
                last_lines = lines[-10:]
                error_msg += " Tycho logs:\n" + "\n".join(last_lines)
            log.exception(error_msg)
            raise Exception("Tycho-client failed.")
        return self._process_message(msg)

    @staticmethod
    def _process_message(msg: dict[str, Any]) -> FeedMessage:
        try:
            return FeedMessage(**msg)
        except ValidationError:
            print(json.dumps(msg, indent=2))
            raise


def get_default_log_directory():
    system = platform.system()

    if system == "Windows":
        default_dir = os.path.join(os.getenv("APPDATA"), "tycho-client", "logs")
    elif system == "Darwin":  # macOS
        default_dir = os.path.join(
            os.path.expanduser("~"), "Library", "Logs", "tycho-client"
        )
    else:  # Linux and other Unix-like systems
        default_dir = os.path.join(
            os.path.expanduser("~"), ".local", "share", "tycho-client", "logs"
        )

    # Create the directory if it doesn't exist
    os.makedirs(default_dir, exist_ok=True)

    return default_dir


def find_tycho_client():
    # Check if 'tycho-client' is in the PATH
    tycho_client_path = shutil.which("tycho-client-cli")

    return tycho_client_path


if __name__ == "__main__":
    stream = TychoStream("localhost:8888", ["uniswap_v2"], Decimal(100), Chain.ethereum)


    async def print_messages():
        await stream.start()
        async for msg in stream:
            print(msg)


    asyncio.run(print_messages())
