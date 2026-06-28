# XELIS Bot

XELIS Bot lets you to send/receive and withdraw/deposit XELIS coins across Discord and Telegram.
This support Slash Commands from Discord.

You also get notified in DM for each deposit confirmed.

Wallet Service is a wrapper around the Wallet to allows easy interactions with it.

Supported commands are:
- `/balance` Show your current balance.
- `/deposit` Show your deposit address.
- `/withdraw` Withdraw XELIS to a wallet on chain.
- `/tip` transfer XELIS to a Discord user.
- `/status` show current wallet service status.

There is no specific requirements like Database setup because it is directly using the Services capabilities from XELIS wallet.

A task in `WalletService` is running and wait on wallet events to handle new incoming transactions.