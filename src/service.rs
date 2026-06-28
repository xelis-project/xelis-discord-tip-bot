use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc
    },
    time::Duration
};

use anyhow::Result;
use poise::serenity_prelude::{Http, CreateMessage, CreateEmbed};
use teloxide::{types::ChatId, Bot};
use thiserror::Error;
use xelis_common::{
    api::{
        daemon::{GetDifficultyResult, GetInfoResult},
        wallet::{EntryType, TransactionEntry},
        DataElement,
        DataValue
    },
    config::XELIS_ASSET,
    crypto::{
        ecdlp::NoOpProgressTableGenerationReportFunction,
        Address,
        Hash,
        Hashable
    },
    network::Network,
    serializer::{Reader, ReaderError, Serializer, Writer},
    transaction::builder::{
        FeeBuilder,
        TransactionTypeBuilder,
        TransferBuilder
    },
    utils::format_xelis
};
use xelis_wallet::{
    error::WalletError,
    precomputed_tables,
    storage::EncryptedStorage,
    wallet::{Event, Wallet}
};
use log::{debug, error, info, warn};

use crate::{telegram_message::TelegramMessage, COLOR, ICON};

const BALANCES_TREE: &str = "balances";
const HISTORY_TREE: &str = "history";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserApplication {
    Telegram(u64),
    Discord(u64)
}

impl Serializer for UserApplication {
    fn write(&self, writer: &mut Writer) {
        match self {
            UserApplication::Telegram(id) => {
                writer.write_u8(0);
                id.write(writer);
            },
            UserApplication::Discord(id) => {
                writer.write_u8(1);
                id.write(writer);
            }
        }
    }

    fn read(reader: &mut Reader) -> Result<Self, ReaderError> {
        let id = match reader.read_u8()? {
            0 => UserApplication::Telegram(reader.read_u64()?),
            1 => UserApplication::Discord(reader.read_u64()?),
            _ => return Err(ReaderError::InvalidValue)
        };

        Ok(id)
    }
}

impl Into<DataValue> for &UserApplication {
    fn into(self) -> DataValue {
        DataValue::Blob(self.to_bytes())
    }
}

impl Into<DataElement> for &UserApplication {
    fn into(self) -> DataElement {
        DataElement::Value(self.into())
    }
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("Cannot transfer 0 XEL")]
    Zero,
    #[error("You can't tip yourself")]
    SelfTip,
    #[error("Not enough funds to transfer {} XEL", format_xelis(*.0))]
    NotEnoughFunds(u64),
    #[error("Not enough funds to pay {} XEL of fee", format_xelis(*.0))]
    NotEnoughFundsForFee(u64),
    #[error("Service is already running")]
    AlreadyRunning,
    #[error("Withdraw is locked")]
    WithdrawLocked,
    #[error(transparent)]
    Any(#[from] anyhow::Error),
    #[error(transparent)]
    WalletError(#[from] WalletError),
    #[error("Wallet is offline")]
    WalletOffline,
}

pub type WalletService = Arc<WalletServiceImpl>;

pub struct XelisStatsSnapshot {
    pub info: GetInfoResult,
    pub difficulty: GetDifficultyResult,
}

pub struct WalletServiceImpl {
    wallet: Arc<Wallet>,
    running: AtomicBool,
    locked: AtomicBool,
}

impl WalletServiceImpl {
    // Create a new wallet service
    pub async fn new(name: &str, password: &str, daemon_address: String, network: Network, n_threads_decryption: usize, network_concurrency: usize) -> Result<WalletService> {
        let precomputed_tables = precomputed_tables::read_or_generate_precomputed_tables(None, precomputed_tables::L1_FULL, NoOpProgressTableGenerationReportFunction, true).await?;

        let wallet = if Path::new(&name).is_dir() {
            Wallet::open(name, password, network, precomputed_tables, n_threads_decryption, network_concurrency)?
        } else {
            Wallet::create(name, password, None, network, precomputed_tables, n_threads_decryption, network_concurrency).await?
        };

        wallet.set_online_mode(&daemon_address, true).await?;

        let service = Arc::new(Self {
            wallet,
            running: AtomicBool::new(false),
            locked: AtomicBool::new(false)
        });

        Ok(service)
    }

    // Start the service to scan all incoming TXs
    pub async fn start(self: WalletService, http: Arc<Http>, bot: Bot) -> Result<(), ServiceError> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(ServiceError::AlreadyRunning);
        }

        tokio::spawn(async move {
            loop {
                info!("Starting event loop");
                if let Err(e) = self.event_loop(&http, &bot).await {
                    error!("Error in event loop: {:?}", e);
                }

                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        Ok(())
    }

    // Notify a discord user of a deposit
    async fn notify_discord_deposit(&self, http: &Http, user_id: u64, amount: u64, transaction_hash: &Hash) -> Result<()> {
        let user = http.get_user(user_id.try_into()?).await?;
        let channel = user.create_dm_channel(&http).await?;

        let embed = CreateEmbed::default()
            .title("Deposit")
            .description(format!("You received {} XEL", format_xelis(amount)))
            .field("Transaction", transaction_hash.to_string(), false)
            .thumbnail(ICON)
            .colour(COLOR);

        channel.send_message(&http, CreateMessage::default().embed(embed)).await?;
        Ok(())
    }

    // Notify a telegram user of a deposit
    async fn notify_telegram_deposit(&self, bot: &Bot, user_id: u64, amount: u64, transaction_hash: &Hash) -> Result<()> {
        TelegramMessage::new(&bot, ChatId(user_id as i64), None)
            .title("Deposit")
            .field("You received", format!("{} XEL", format_xelis(amount)), false)
            .field("Transaction", transaction_hash.to_string(), false)
            .send().await?;

        Ok(())
    }

    // Handle a confirmed transaction
    // This function is called when a transaction is in stable topoheight
    async fn handle_confirmed_transaction(&self, transaction: &TransactionEntry<'static>, http: &Http, bot: &Bot) -> Result<()> {
        match &transaction.entry {
            EntryType::Incoming { from: _, transfers } => {
                // Check if there is any transfer that is for us
                for transfer in transfers.iter().filter(|t| t.asset.as_ref() == &XELIS_ASSET) {
                    if let Some(data) = transfer.extra_data.as_ref() {
                        if let Some(user_id) = data.data().and_then(|v| v.as_value().and_then(|v| v.as_type::<UserApplication>()).ok()) {
                            let amount = transfer.amount;
                            {
                                let mut storage = self.wallet.get_storage().write().await;
                                let tx_key = transaction.hash.as_ref().clone().into();
                                if storage.has_custom_data(HISTORY_TREE, &tx_key)? {
                                    // Already processed this TX
                                    info!("Already processed TX: {}", transaction.hash);
                                    continue;
                                }

                                info!("Processing TX: {}", transaction.hash);
                                // Calculate new balance
                                let balance = self.get_balance_internal(&storage, &user_id);
                                let new_balance = balance + amount;
                                // Update balance
                                storage.set_custom_data(BALANCES_TREE, &(&user_id).into(), &new_balance.into())?;

                                // Store the TX hash in the history
                                storage.set_custom_data(HISTORY_TREE, &tx_key, &(&user_id).into())?;
                            }

                            info!("User {:?} received {} XEL in TX {}", user_id, format_xelis(amount), transaction.hash);
                            // Notify user
                            match user_id {
                                UserApplication::Telegram(user_id) => {
                                    if let Err(e) = self.notify_telegram_deposit(&bot, user_id, amount, &transaction.hash).await {
                                        error!("Error while notifying user of deposit: {:?}", e);
                                    }
                                },
                                UserApplication::Discord(user_id) => {
                                    if let Err(e) = self.notify_discord_deposit(&http, user_id, amount, &transaction.hash).await {
                                        error!("Error while notifying user of deposit: {:?}", e);
                                    }
                                }
                            }
                        } else {
                            warn!("Invalid user application data: {:?}", data);
                        }
                    }
                }
            },
            _ => {}
        }

        Ok(())
    }

    // this function is called one time at WalletService creation,
    // and is notified by the wallet of any new transaction
    async fn event_loop(self: &WalletService, http: &Arc<Http>, bot: &Bot) -> Result<()> {
        // Get all unconfirmed transactions
        let mut unconfirmed_transactions: VecDeque<TransactionEntry> = VecDeque::new();

        // Receiver for wallet events
        let mut receiver = self.wallet.subscribe_events().await;

        // Receiver for stable topoheight changes
        let mut stable_topoheight_receiver = {
            let lock = self.wallet.get_network_handler();
            let network_handler = lock.lock().await;

            if let Some(network_handler) = network_handler.as_ref() {
                network_handler.get_api().on_stable_topoheight_changed_event().await?
            } else {
                return Err(ServiceError::WalletOffline.into());
            }
        };

        // Handle events
        loop {
            tokio::select! {
                res = stable_topoheight_receiver.next() => {
                    let event = res?;

                    // Handle all transactions that are now confirmed
                    while let Some(transaction) = unconfirmed_transactions.pop_front() {
                        if transaction.topoheight <= event.new_stable_topoheight {
                            self.handle_confirmed_transaction(&transaction, http, bot).await?;
                        } else {
                            info!("Re-adding TX to unconfirmed transactions: {}", transaction.hash);
                            unconfirmed_transactions.push_front(transaction);
                            break;
                        }
                    }
                },
                res = receiver.recv() => {
                    let event = res?;
                    match event {
                        Event::NewTransaction(transaction) => {
                            info!("New transaction: {}", transaction.hash);
                            if unconfirmed_transactions.iter().any(|t| t.hash == transaction.hash) {
                                warn!("TX already in unconfirmed transactions: {}", transaction.hash);
                                continue;
                            }

                            unconfirmed_transactions.push_back(transaction);
                        }
                        Event::Rescan { start_topoheight: _ } => {
                            warn!("Rescan event received, this should not happen");
                            self.locked.store(true, Ordering::SeqCst);
                        },
                        _ => {}
                    }
                }
            }
        }
    }

    // Get the balance for a user based on its id
    fn get_balance_internal(&self, storage: &EncryptedStorage, user: &UserApplication) -> u64 {
        let balance = match storage.get_custom_data(BALANCES_TREE, &DataValue::Blob(user.to_bytes())) {
            Ok(balance) => balance,
            Err(_) => return 0
        };
        let balance = balance.to_value().map(|v| v.to_u64().unwrap_or(0)).unwrap_or(0);
        balance
    }

    // Get the balance for a user based on its id
    pub async fn get_balance_for_user(&self, user: &UserApplication) -> u64 {
        let storage = self.wallet.get_storage().read().await;
        self.get_balance_internal(&storage, user)
    }

    // Get the total balance for all users
    pub async fn get_total_users_balance(&self) -> Result<u64> {
        let storage = self.wallet.get_storage().read().await;
        let mut total = 0;
        for key in storage.get_custom_tree_keys(&BALANCES_TREE.to_string(), &None, None, None)? {
            let user_id = key.to_type()?;
            debug!("Getting balance for discord key: {:?}", user_id);
            let balance: u64 = self.get_balance_internal(&storage, &user_id);
            total += balance;
        }

        Ok(total)
    }

    // Get the balance for the service
    pub async fn get_wallet_balance(&self) -> Result<u64> {
        let storage = self.wallet.get_storage().read().await;
        let balance = storage.get_plaintext_balance_for(&XELIS_ASSET).await.unwrap_or(0);
        Ok(balance)
    }

    // Get the current wallet topoheight
    pub async fn get_wallet_topoheight(&self) -> Result<u64> {
        let storage = self.wallet.get_storage().read().await;
        let topoheight = storage.get_synced_topoheight()?;
        Ok(topoheight)
    }

    // Get public chain stats using the daemon API already owned by the wallet
    pub async fn get_xelis_stats_snapshot(&self) -> Result<XelisStatsSnapshot, ServiceError> {
        let network_handler = {
            let lock = self.wallet.get_network_handler();
            let network_handler = lock.lock().await;
            network_handler.as_ref().cloned().ok_or(ServiceError::WalletOffline)?
        };

        let api = network_handler.get_api();
        let info = api.get_info().await?;
        let difficulty = api.client()
            .call("get_difficulty")
            .await
            .map_err(anyhow::Error::from)?;

        Ok(XelisStatsSnapshot {
            info,
            difficulty,
        })
    }

    // Generate a deposit address for a user based on its id
    pub fn get_address_for_user(&self, user: &UserApplication) -> Address {
        self.wallet.get_address_with(DataElement::Value(DataValue::Blob(user.to_bytes())))
    }

    // Transfer XEL from one user to another
    pub async fn transfer(&self, from: &UserApplication, to: &UserApplication, amount: u64) -> Result<(), ServiceError> {
        if amount == 0 {
            return Err(ServiceError::Zero);
        }

        if from == to {
            return Err(ServiceError::SelfTip);
        }

        let mut storage = self.wallet.get_storage().write().await;
        let from_balance = self.get_balance_internal(&storage, &from);
        if amount > from_balance {
            return Err(ServiceError::NotEnoughFunds(amount));
        }

        let to_balance = self.get_balance_internal(&storage, to);

        // Update balances
        storage.set_custom_data(BALANCES_TREE, &from.into(), &(from_balance - amount).into())?;
        storage.set_custom_data(BALANCES_TREE, &to.into(), &(to_balance + amount).into())?;

        Ok(())
    }

    // Withdraw XEL from the service to an address
    pub async fn withdraw(&self, user: &UserApplication, to: Address, amount: u64) -> Result<Hash, ServiceError> {
        if amount == 0 {
            return Err(ServiceError::Zero);
        }

        if self.locked.load(Ordering::SeqCst) {
            return Err(ServiceError::WithdrawLocked);
        }

        let builder = TransactionTypeBuilder::Transfers(vec![TransferBuilder {
                amount,
                asset: XELIS_ASSET,
                destination: to.clone(),
                extra_data: None,
                encrypt_extra_data: true,
            }
        ]);

        let fee = self.wallet.estimate_fees(builder.clone(), Default::default(), Default::default()).await?;

        let mut storage = self.wallet.get_storage().write().await;
        let (balance, fee, mut state, transaction) = {
            let balance = self.get_balance_internal(&storage, user);
            if amount > balance {
                return Err(ServiceError::NotEnoughFunds(amount));
            }

            // Verify if he has enough with fees included
            if fee + amount > balance {
                return Err(ServiceError::NotEnoughFundsForFee(fee));
            }

            let (transaction, state) = self.wallet.create_transaction_with_storage(
                &storage,
                builder,
                FeeBuilder::Fixed(fee),
                Default::default(),
                None
            ).await?;

            (balance, fee, state, transaction)
        };

        self.wallet.submit_transaction(&transaction).await?;

        let tx_hash = transaction.hash();
        info!("Withdrawing {} XEL to {} in TX {} from {:?}", format_xelis(amount), to, tx_hash, user);

        // Update balance
        storage.set_custom_data(BALANCES_TREE, &user.into(), &(balance - (fee + amount)).into())?;
        state.apply_changes(&mut storage, &self.wallet, &transaction).await?;

        Ok(tx_hash)
    }

    // Clear the balances tree
    // This will remove all user balances
    pub async fn clear_balances(&self) -> Result<(), ServiceError> {
        warn!("Clearing balances tree");
        let mut storage = self.wallet.get_storage().write().await;
        storage.clear_custom_tree(BALANCES_TREE)?;

        Ok(())
    }

    // Add balance to a user
    pub async fn add_balance(&self, user: &UserApplication, amount: u64) -> Result<(), ServiceError> {
        warn!("Adding {} XEL to {:?}", format_xelis(amount), user);
        let mut storage = self.wallet.get_storage().write().await;
        let balance = self.get_balance_internal(&storage, user);
        storage.set_custom_data(BALANCES_TREE, &user.into(), &(balance + amount).into())?;

        Ok(())
    }

    pub async fn remove_balance(&self, user: &UserApplication, amount: u64) -> Result<(), ServiceError> {
        warn!("Removing {} XEL from {:?}", format_xelis(amount), user);
        let mut storage = self.wallet.get_storage().write().await;
        let balance = self.get_balance_internal(&storage, user);
        if amount > balance {
            return Err(ServiceError::NotEnoughFunds(amount));
        }
        storage.set_custom_data(BALANCES_TREE, &user.into(), &(balance - amount).into())?;

        Ok(())
    }

    // Withdraw all XEL from the service to an address
    pub async fn withdraw_all(&self, to: Address) -> Result<(), ServiceError> {
        let amount = {
            let storage = self.wallet.get_storage().read().await;
            storage.get_plaintext_balance_for(&XELIS_ASSET).await.unwrap_or(0)
        };

        self.withdraw_to(to, amount).await
    }

    // Withdraw XEL from the service to an address
    pub async fn withdraw_to(&self, to: Address, amount: u64) -> Result<(), ServiceError> {
        let fee = self.wallet.estimate_fees(
            TransactionTypeBuilder::Transfers(vec![TransferBuilder {
                amount,
                asset: XELIS_ASSET,
                destination: to.clone(),
                extra_data: None,
                encrypt_extra_data: true,
            }]),
            Default::default(),
            Default::default()
        ).await?;

        let mut storage = self.wallet.get_storage().write().await;
        let (transaction, mut state) = self.wallet.create_transaction_with_storage(
            &storage,
            TransactionTypeBuilder::Transfers(vec![TransferBuilder {
                amount: amount - fee,
                asset: XELIS_ASSET,
                destination: to.clone(),
                extra_data: None,
                encrypt_extra_data: true,
            }]),
            FeeBuilder::Fixed(fee),
            Default::default(),
            None
        ).await?;

        self.wallet.submit_transaction(&transaction).await?;

        let tx_hash = transaction.hash();
        info!("Withdrawing {} XEL to {} in TX {}", format_xelis(amount - fee), to, tx_hash);

        state.apply_changes(&mut storage, &self.wallet, &transaction).await?;

        Ok(())
    }

    // Get the network of the wallet
    pub fn network(&self) -> &Network {
        self.wallet.get_network()
    }

    // Check if the wallet is online
    pub async fn is_wallet_online(&self) -> bool {
        self.wallet.is_online().await
    }

    // Rescan the wallet
    pub async fn rescan(&self) -> Result<(), ServiceError> {
        self.wallet.rescan(0, true).await?;
        Ok(())
    }
}