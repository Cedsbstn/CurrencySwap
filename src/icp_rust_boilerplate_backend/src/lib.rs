#[macro_use]
extern crate serde;
use candid::{Decode, Encode};
use ic_cdk::api::caller;
use ic_cdk::api::time;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{BoundedStorable, Cell, DefaultMemoryImpl, StableBTreeMap, Storable};
use lazy_static::lazy_static;
use regex::Regex;
use std::borrow::Cow;
use std::cell::RefCell;

type Memory = VirtualMemory<DefaultMemoryImpl>;
type BalanceCell = Cell<u64, Memory>;

#[derive(candid::CandidType, Clone, Serialize, Deserialize, Default, Debug)]
struct UserAccount {
    balance: u64, // balance in smallest denomination
}

impl Storable for UserAccount {
    fn to_bytes(&self) -> Cow<[u8]> {
        Cow::Owned(Encode!(self).expect("Failed to encode UserAccount"))
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("Failed to decode UserAccount")
    }
}

impl BoundedStorable for UserAccount {
    const MAX_SIZE: u32 = 128;
    const IS_FIXED_SIZE: bool = false;
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StorablePrincipal(candid::Principal);

impl From<candid::Principal> for StorablePrincipal {
    fn from(principal: candid::Principal) -> Self {
        StorablePrincipal(principal)
    }
}

impl From<StorablePrincipal> for candid::Principal {
    fn from(storable: StorablePrincipal) -> Self {
        storable.0
    }
}

impl Storable for StorablePrincipal {
    fn to_bytes(&self) -> Cow<[u8]> {
        Cow::Owned(self.0.as_slice().to_vec())
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        StorablePrincipal(candid::Principal::from_slice(&bytes))
    }
}

impl BoundedStorable for StorablePrincipal {
    const MAX_SIZE: u32 = 29;
    const IS_FIXED_SIZE: bool = false;
}

#[derive(candid::CandidType, Clone, Serialize, Deserialize)]
enum OrderType {
    Market,
    Limit { price: f64 },
}

impl Default for OrderType {
    fn default() -> Self {
        OrderType::Market
    }
}

#[derive(candid::CandidType, Clone, Serialize, Deserialize)]
struct SwapOrder {
    id: u64,
    owner: candid::Principal,
    from_currency: String,
    to_currency: String,
    from_amount: u64,
    to_amount: u64,
    order_type: OrderType,
    created_at: u64,
    status: SwapStatus,
}

impl Default for SwapOrder {
    fn default() -> Self {
        SwapOrder {
            id: 0,
            owner: candid::Principal::anonymous(),
            from_currency: String::default(),
            to_currency: String::default(),
            from_amount: 0,
            to_amount: 0,
            order_type: OrderType::default(),
            created_at: 0,
            status: SwapStatus::default(),
        }
    }
}

#[derive(candid::CandidType, Clone, Serialize, Deserialize, PartialEq)]
enum SwapStatus {
    Created,
    Executed,
    Cancelled,
}

impl Default for SwapStatus {
    fn default() -> Self {
        SwapStatus::Created
    }
}

impl Storable for SwapOrder {
    fn to_bytes(&self) -> Cow<[u8]> {
        Cow::Owned(Encode!(self).expect("Failed to encode SwapOrder"))
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("Failed to decode SwapOrder")
    }
}

impl BoundedStorable for SwapOrder {
    const MAX_SIZE: u32 = 512;
    const IS_FIXED_SIZE: bool = false;
}

thread_local! {
    static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> = RefCell::new(
        MemoryManager::init(DefaultMemoryImpl::default())
    );

    static USER_ACCOUNTS: RefCell<StableBTreeMap<StorablePrincipal, UserAccount, Memory>> =
        RefCell::new(StableBTreeMap::init(
            MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(0)))
    ));

    static SWAP_ORDERS: RefCell<StableBTreeMap<u64, SwapOrder, Memory>> =
        RefCell::new(StableBTreeMap::init(
            MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(1)))
    ));

    static ORDER_COUNTER: RefCell<BalanceCell> = RefCell::new(
        BalanceCell::init(MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(2))), 0)
            .expect("Cannot create a counter")
    );
}

#[derive(candid::CandidType, Serialize, Deserialize)]
struct DepositArgs {
    amount: u64,
    currency: String,
}

#[ic_cdk::update]
fn deposit(args: DepositArgs) -> Result<(), Error> {
    if args.amount == 0 {
        return Err(Error::InvalidAmount);
    }
    if !is_valid_currency(&args.currency) {
        return Err(Error::InvalidCurrency);
    }

    let caller_principal = StorablePrincipal::from(caller());
    USER_ACCOUNTS.with(|accounts| {
        let mut accounts_borrowed = accounts.borrow_mut();
        let mut user_account = accounts_borrowed.get(&caller_principal).as_ref().cloned().unwrap_or_default();
        user_account.balance += args.amount;
        accounts_borrowed.insert(caller_principal, user_account);
    });

    Ok(())
}

#[derive(candid::CandidType, Serialize, Deserialize)]
struct CreateSwapOrderArgs {
    from_currency: String,
    to_currency: String,
    from_amount: u64,
    to_amount: u64,
    order_type: OrderType,
}

#[ic_cdk::update]
fn create_swap_order(args: CreateSwapOrderArgs) -> Result<u64, Error> {
    if args.from_amount == 0 || args.to_amount == 0 {
        return Err(Error::InvalidAmount);
    }
    if !is_valid_currency(&args.from_currency) || !is_valid_currency(&args.to_currency) {
        return Err(Error::InvalidCurrency);
    }
    if let OrderType::Limit { price } = &args.order_type {
        if *price <= 0.0 {
            return Err(Error::InvalidPrice);
        }
    }

    let caller_principal = StorablePrincipal::from(caller());
    let _user_account = USER_ACCOUNTS.with(|accounts| {
        let mut accounts_borrowed = accounts.borrow_mut();
        let mut user_account = accounts_borrowed.get(&caller_principal).as_ref().cloned().unwrap_or_else(|| {
            accounts_borrowed.insert(caller_principal.clone(), UserAccount::default());
            accounts_borrowed.get(&caller_principal).as_ref().cloned().unwrap()
        });

        if user_account.balance < args.from_amount {
            Err(Error::InsufficientFunds)
        } else {
            user_account.balance -= args.from_amount;
            accounts_borrowed.insert(caller_principal, user_account.clone());
            Ok(user_account)
        }
    })?;

    let order_id = ORDER_COUNTER.with(|counter| -> Result<u64, Error> {
        let binding = counter.borrow();
        let current_value = binding.get();
        let new_value = current_value + 1;
        counter.borrow_mut().set(new_value).map_err(|_| Error::InvalidAmount)?;
        Ok(new_value)
    })?;
       
    let swap_order = SwapOrder {
        id: order_id,
        owner: caller(),
        from_currency: args.from_currency,
        to_currency: args.to_currency,
        from_amount: args.from_amount,
        to_amount: args.to_amount,
        order_type: args.order_type,
        created_at: time(),
        status: SwapStatus::Created,
    };

    SWAP_ORDERS.with(|orders| orders.borrow_mut().insert(order_id, swap_order));

    Ok(order_id)
}

#[ic_cdk::update]
fn execute_swap_order(order_id: u64) -> Result<(), Error> {
    let executor_principal = StorablePrincipal::from(caller());
    if executor_principal == StorablePrincipal::from(candid::Principal::anonymous()) {
        return Err(Error::AnonymousNotAllowed);
    }

    let mut swap_order = SWAP_ORDERS.with(|orders| orders.borrow().get(&order_id).as_ref().cloned())
        .ok_or(Error::InvalidOrderId)?;

    if swap_order.status != SwapStatus::Created {
        return Err(Error::InvalidOrderStatus);
    }

    let owner_principal = StorablePrincipal::from(swap_order.owner);

    if owner_principal == executor_principal {
        return Err(Error::OwnerCannotExecute);
    }

    let transfer_result = match swap_order.order_type {
        OrderType::Market => {
            // For market orders, execute immediately
            transfer_funds(executor_principal, owner_principal, swap_order.to_amount)
        }
        OrderType::Limit { price } => {
            // For limit orders, check if the price condition is met
            if is_price_condition_met(price) {
                transfer_funds(executor_principal, owner_principal, swap_order.to_amount)
            } else {
                Err(Error::PriceConditionNotMet)
            }
        }
    };

    match transfer_result {
        Ok(()) => {
            swap_order.status = SwapStatus::Executed;
        }
        Err(err) => return Err(err),
    }

    SWAP_ORDERS.with(|orders| orders.borrow_mut().insert(order_id, swap_order));

    Ok(())
}

fn transfer_funds(from: StorablePrincipal, to: StorablePrincipal, amount: u64) -> Result<(), Error> {
    if amount == 0 {
        return Ok(()); // No need to transfer if the amount is zero
    }
    if from == to {
        return Ok(()); // No need to transfer to self
    }

    USER_ACCOUNTS.with(|accounts| {
        let mut accounts_borrowed = accounts.borrow_mut();
        let mut from_account = accounts_borrowed.get(&from).as_ref().cloned().ok_or(Error::UserNotFound)?;
        let mut to_account = accounts_borrowed.get(&to).as_ref().cloned().unwrap_or_else(|| {
            accounts_borrowed.insert(to.clone(), UserAccount::default());
            accounts_borrowed.get(&to).as_ref().cloned().unwrap()
        });

        if from_account.balance < amount {
            Err(Error::InsufficientFunds)
        } else {
            from_account.balance -= amount;
            to_account.balance += amount;
            accounts_borrowed.insert(from.clone(), from_account);
            accounts_borrowed.insert(to.clone(), to_account);
            Ok(())
        }
    })
}

#[ic_cdk::update]
fn cancel_swap_order(order_id: u64) -> Result<(), Error> {
    let caller_principal = StorablePrincipal::from(caller());
    let mut swap_order = SWAP_ORDERS.with(|orders| orders.borrow_mut().get(&order_id).as_ref().cloned())
        .ok_or(Error::InvalidOrderId)?;

    if swap_order.status != SwapStatus::Created {
        return Err(Error::InvalidOrderStatus);
    }

    if swap_order.owner != caller_principal.clone().into() {
        return Err(Error::Unauthorized);
    }

    USER_ACCOUNTS.with(|accounts| {
        let mut accounts_borrowed = accounts.borrow_mut();
        let mut owner_account = accounts_borrowed.get(&caller_principal).as_ref().cloned().unwrap_or_else(|| {
            accounts_borrowed.insert(caller_principal.clone(), UserAccount::default());
            accounts_borrowed.get(&caller_principal).as_ref().cloned().unwrap()
        });
        owner_account.balance += swap_order.from_amount;
        accounts_borrowed.insert(caller_principal, owner_account);
        Ok(())
    })?;

    swap_order.status = SwapStatus::Cancelled;
    SWAP_ORDERS.with(|orders| orders.borrow_mut().insert(order_id, swap_order));

    Ok(())
}

#[ic_cdk::query]
fn get_user_balance() -> Option<u64> {
    let caller_principal = StorablePrincipal::from(caller());
    USER_ACCOUNTS.with(|accounts| accounts.borrow().get(&caller_principal).as_ref().cloned())
        .map(|account| account.balance)
}

#[ic_cdk::query]
fn get_swap_order(order_id: u64) -> Option<SwapOrder> {
    SWAP_ORDERS.with(|orders| orders.borrow().get(&order_id).as_ref().cloned())
}

// Placeholder function to simulate price condition checking
fn is_price_condition_met(price: f64) -> bool {
    // Simulate a price check
    price <= 1.2 // Example condition
}

lazy_static! {
    static ref CURRENCY_REGEX: Regex = Regex::new(r"^[A-Z]{3}$").unwrap();
}

fn is_valid_currency(currency: &str) -> bool {
    CURRENCY_REGEX.is_match(currency)
}

#[derive(candid::CandidType, Deserialize, Serialize, Debug, PartialEq)]
enum Error {
    InsufficientFunds,
    InvalidOrderId,
    InvalidOrderStatus,
    Unauthorized,
    UserNotFound,
    PriceConditionNotMet,
    InvalidAmount,
    InvalidCurrency,
    InvalidPrice,
    AnonymousNotAllowed,
    OwnerCannotExecute,
}

// need this to generate candid
ic_cdk::export_candid!();
