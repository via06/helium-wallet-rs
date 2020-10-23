use crate::{
    cmd::{
        api_url, get_password, load_wallet, print_footer, print_json, status_json, status_str,
        Opts, OutputFormat,
    },
    result::Result,
    traits::{Sign, Signer, TxnEnvelope, B64},
};
use helium_api::{BlockchainTxn, BlockchainTxnPriceOracleV1, Client, PendingTxnStatus};
use rust_decimal::{prelude::*, Decimal};
use serde::Serialize;
use serde_json::json;
use std::{cmp, str::FromStr};
use structopt::StructOpt;

/// Report an oracle price to the blockchain
#[derive(Debug, StructOpt)]
pub enum Cmd {
    Report(Report),
    ReportWeightedAverage(ReportWeightedAverage),
    Automate(AutomatedReportByWeightedAverage),
}

#[derive(Debug, StructOpt)]
/// Construct an oracle price report and optionally commit it to the
/// Helium Blockchain.
pub struct Report {
    /// The oracle price to report. Specify in USD or supply one of the
    /// supported price lookup services ("coingecko", "bilaxy", "binance").
    #[structopt(long)]
    price: Price,

    /// Block height to report the price at. Use "auto" to pick the
    /// latest known block height from the API.
    #[structopt(long)]
    block: Block,

    /// Commit the oracle price report to the API
    #[structopt(long)]
    commit: bool,
}

#[derive(Debug, StructOpt)]
/// Construct an oracle price report by averaging prices. Weights are accepted
/// as arbitrary floats.
pub struct ReportWeightedAverage {
    /// Optional block height to report the price at.
    /// Omit to use latest known block height from the API.
    #[structopt(long)]
    block: Option<u64>,

    /// Weight given for Binance US price
    #[structopt(long, default_value = "0")]
    binance_us: f32,

    /// Weight given for Binance International price
    #[structopt(long, default_value = "0")]
    binance_int: f32,

    /// Weight given for Bilaxy price
    #[structopt(long, default_value = "0")]
    bilaxy: f32,

    /// Weight given for Coingecko price
    #[structopt(long, default_value = "0")]
    coingecko: f32,
}

#[derive(Debug, StructOpt)]
/// Construct an oracle price report by averaging prices. Weights are accepted
/// as arbitrary floats. User inputs for randomized delay between submissions.
pub struct AutomatedReportByWeightedAverage {
    /// Average delay between price submissions
    #[structopt(long, default_value = "15")]
    delay: u16, // constrain to 16 bit int for range

    /// Standard dev between price submissions
    #[structopt(long, default_value = "8")]
    std_dev: u16, // constrain to 16 bit int for range

    /// Min time between price submissions
    #[structopt(long, default_value = "8")]
    min: u16, // constrain to 16 bit int for range

    /// Weight given for Binance US price
    #[structopt(long, default_value = "0")]
    binance_us: f32,

    /// Weight given for Binance International price
    #[structopt(long, default_value = "0")]
    binance_int: f32,

    /// Weight given for Bilaxy price
    #[structopt(long, default_value = "0")]
    bilaxy: f32,

    /// Weight given for Coingecko price
    #[structopt(long, default_value = "0")]
    coingecko: f32,
}

impl Cmd {
    pub fn run(&self, opts: Opts) -> Result {
        match self {
            Cmd::Report(cmd) => cmd.run(opts),
            Cmd::ReportWeightedAverage(cmd) => cmd.run(opts),
            Cmd::Automate(cmd) => cmd.run(opts),
        }
    }
}

impl Report {
    pub fn run(&self, opts: Opts) -> Result {
        let password = get_password(false)?;
        let wallet = load_wallet(opts.files)?;
        let keypair = wallet.decrypt(password.as_bytes())?;

        let client = Client::new_with_base_url(api_url());

        let mut txn = BlockchainTxnPriceOracleV1 {
            public_key: keypair.pubkey_bin().into(),
            price: self.price.to_millis(),
            block_height: self.block.to_block(),
            signature: Vec::new(),
        };

        let envelope = txn.sign(&keypair, Signer::Owner)?.in_envelope();
        let status = if self.commit {
            Some(client.submit_txn(&envelope)?)
        } else {
            None
        };

        print_txn(&txn, &envelope, &status, &opts.format)
    }
}

impl ReportWeightedAverage {
    pub fn run(&self, opts: Opts) -> Result {
        let weights = Weights {
            binance_us: self.binance_us,
            binance_int: self.binance_int,
            bilaxy: self.bilaxy,
            coingecko: self.coingecko,
        };

        let price = Price::from_weights(&weights)?;

        let client = Client::new_with_base_url(api_url());
        let block_height = if let Some(block) = self.block {
            block
        } else {
            client.get_height()?
        };

        println!(
            "Report price {:?} @ block height {}?",
            price.0, block_height
        );
        println!("Enter password to confirm.");

        let password = get_password(false)?;
        let wallet = load_wallet(opts.files)?;
        let keypair = wallet.decrypt(password.as_bytes())?;

        let mut txn = BlockchainTxnPriceOracleV1 {
            public_key: keypair.pubkey_bin().into(),
            price: price.to_millis(),
            block_height,
            signature: Vec::new(),
        };

        let envelope = txn.sign(&keypair, Signer::Owner)?.in_envelope();
        let status = Some(client.submit_txn(&envelope)?);

        print_txn(&txn, &envelope, &status, &opts.format)
    }
}

use rand::thread_rng;
use rand_distr::{Distribution, Normal};

impl AutomatedReportByWeightedAverage {
    pub fn run(&self, opts: Opts) -> Result {
        use std::{thread::sleep, time};
        let mut rng = thread_rng();

        let distribution = Normal::new(self.delay as f32, self.std_dev as f32)?;

        let weights = Weights {
            binance_us: self.binance_us,
            binance_int: self.binance_int,
            bilaxy: self.bilaxy,
            coingecko: self.coingecko,
        };

        println!(
            "Starting oracle report automation with the following weights:\n{:?}",
            weights
        );

        println!("Enter password to start utility.");

        let password = get_password(false)?;
        let wallet = load_wallet(opts.files)?;
        let keypair = wallet.decrypt(password.as_bytes())?;

        loop {
            let price = Price::from_weights(&weights)?;

            let client = Client::new_with_base_url(api_url());
            let block_height =
                retry_fn(Fixed::from_millis(1000).take(10), || client.get_height()).unwrap();

            let mut txn = BlockchainTxnPriceOracleV1 {
                public_key: keypair.pubkey_bin().into(),
                price: price.to_millis(),
                block_height,
                signature: Vec::new(),
            };

            let envelope = txn.sign(&keypair, Signer::Owner)?.in_envelope();
            let status = Some(client.submit_txn(&envelope)?);

            print_txn(&txn, &envelope, &status, &opts.format)?;

            let delay_mins = cmp::min(self.min, distribution.sample(&mut rng) as u16) as u64;
            println!("Next report will be in {}", delay_mins);
            let minutes = time::Duration::from_secs(delay_mins * 60);
            sleep(minutes);
        }
    }
}

fn print_txn(
    txn: &BlockchainTxnPriceOracleV1,
    envelope: &BlockchainTxn,
    status: &Option<PendingTxnStatus>,
    format: &OutputFormat,
) -> Result {
    let encoded = envelope.to_b64()?;
    match format {
        OutputFormat::Table => {
            ptable!(
                ["Key", "Value"],
                ["Block Height", txn.block_height],
                ["Price", Price::from_millis(txn.price)],
                ["Hash", status_str(status)]
            );

            print_footer(status)
        }
        OutputFormat::Json => {
            let table = json!({
                "price": txn.price,
                "block_height": txn.block_height,
                "txn": encoded,
                "hash": status_json(status)
            });
            print_json(&table)
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Block(u64);

impl FromStr for Block {
    type Err = Box<dyn std::error::Error>;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "auto" => {
                let client = Client::new_with_base_url(api_url());
                Ok(Block(client.get_height()?))
            }
            _ => Ok(Block(s.parse()?)),
        }
    }
}

impl Block {
    fn to_block(self) -> u64 {
        self.0
    }
}

const USD_TO_PRICE_SCALAR: u64 = 100_000_000;

#[derive(Clone, Copy, Debug, Serialize)]
struct Price(Decimal);

#[derive(Debug, StructOpt)]
struct Weights {
    binance_us: f32,
    binance_int: f32,
    bilaxy: f32,
    coingecko: f32,
}

// general retry macro for API calls
use retry::{delay::Fixed, retry as retry_fn};
macro_rules! retry {
    ( $x:expr ) => {{
        retry_fn(Fixed::from_millis(1000).take(10), $x)
    }};
}

// macro for trying to fetch, puts 0 if API fails
// also returns 0 without fetch is weight is 0
macro_rules! fetch_or_null {
    ($name:literal, $val:expr, $fetch:expr ) => {{
        if $val != 0.0 {
            match retry!($fetch) {
                Ok(price) => {
                    println!("{:25} reports price of ${}", $name, price.0);
                    ($val, price)
                }
                Err(_err) => {
                    println!(
                        "Warning: {} is failing so removed from weighted average",
                        $name
                    );
                    (0.0, Price::null())
                }
            }
        } else {
            (0.0, Price::null())
        }
    }};
}
impl Price {
    fn null() -> Price {
        Price(Decimal::from_f32(0.0).unwrap())
    }

    fn from_weights(weights: &Weights) -> Result<Self> {
        let mut values = [
            fetch_or_null!("Binance US", weights.binance_us, Price::from_binance_us),
            fetch_or_null!(
                "Binance International",
                weights.binance_int,
                Price::from_binance_int
            ),
            fetch_or_null!("Bilaxy", weights.bilaxy, Price::from_bilaxy),
            fetch_or_null!("Coingecko", weights.coingecko, Price::from_coingecko),
        ];

        let mut price = Price::null();

        let mut sum_weights = 0.0;
        for value in values.iter_mut() {
            sum_weights += value.0;
            value.1.scale(value.0);
            price += value.1;
        }

        if sum_weights == 0.0 {
            panic!("Must have at least one price source! Use --help for more details");
        }
        let scalar = 1.0 / sum_weights;
        price.scale(scalar);
        Ok(price)
    }

    fn scale(&mut self, scalar: f32) {
        self.0 *= Decimal::from_f32(scalar).unwrap();
    }

    fn from_coingecko() -> Result<Self> {
        let mut response = reqwest::get("https://api.coingecko.com/api/v3/coins/helium")?;
        let json: serde_json::Value = response.json()?;
        let amount = &json["market_data"]["current_price"]["usd"];
        Price::from_str(&amount.to_string())
    }

    fn from_bilaxy() -> Result<Self> {
        let mut response = reqwest::get("https://newapi.bilaxy.com/v1/valuation?currency=HNT")?;
        let json: serde_json::Value = response.json()?;
        let amount = &json["HNT"]["usd_value"];
        Price::from_str(amount.as_str().ok_or("No USD value found")?)
    }

    fn from_binance_us() -> Result<Self> {
        let mut response =
            reqwest::get("https://api.binance.us/api/v3/ticker/price?symbol=HNTUSD")?;
        let json: serde_json::Value = response.json()?;
        let amount = &json["price"];
        Price::from_str(amount.as_str().ok_or("No USD value found")?)
    }

    fn from_binance_int() -> Result<Self> {
        let mut response = reqwest::get("https://api.binance.us/api/v3/avgPrice?symbol=HNTUSDT")?;
        let json: serde_json::Value = response.json()?;
        let amount = &json["price"];
        Price::from_str(amount.as_str().ok_or("No USD value found")?)
    }

    fn to_millis(self) -> u64 {
        if let Some(scaled_dec) = self.0.checked_mul(USD_TO_PRICE_SCALAR.into()) {
            if let Some(num) = scaled_dec.to_u64() {
                return num;
            }
        }
        panic!("Price has been constructed with invalid data")
    }

    fn from_millis(millis: u64) -> Self {
        if let Some(mut data) = Decimal::from_u64(millis) {
            data.set_scale(8).unwrap();
            return Price(data);
        }
        panic!("Price value could not be parsed into Decimal")
    }
}

use std::ops::AddAssign;
impl AddAssign for Price {
    fn add_assign(&mut self, other: Price) {
        self.0 += other.0;
    }
}

impl FromStr for Price {
    type Err = Box<dyn std::error::Error>;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "coingecko" => Price::from_coingecko(),
            "bilaxy" => Price::from_bilaxy(),
            // don't break old interface so maintain "binance" to Binance US
            "binance" => Price::from_binance_us(),
            "binance-us" => Price::from_binance_us(),
            "binance-int" => Price::from_binance_int(),
            _ => {
                let data = Decimal::from_str(s).or_else(|_| Decimal::from_scientific(s))?;
                Ok(Price(
                    data.round_dp_with_strategy(8, RoundingStrategy::RoundHalfUp),
                ))
            }
        }
    }
}

impl ToString for Price {
    fn to_string(&self) -> String {
        self.0.to_string()
    }
}
