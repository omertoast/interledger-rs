//! # interledger-btp
//!
//! Client and server implementations of the [Bilateral Transport Protocol (BTP)](https://github.com/interledger/rfcs/blob/master/0023-bilateral-transfer-protocol/0023-bilateral-transfer-protocol.md).
//! This is a WebSocket-based protocol for exchanging ILP packets between directly connected peers.
//!
//! Because this protocol uses WebSockets, only one party needs to have a publicly-accessible HTTPS
//! endpoint but both sides can send and receive ILP packets.

#[macro_use]
extern crate quick_error;
#[cfg(test)]
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;

use futures::Future;
use interledger_service::Account;
use url::Url;

mod client;
mod errors;
mod oer;
mod packet;
mod server;
mod service;

pub use self::client::{connect_client, parse_btp_url};
pub use self::server::{create_open_signup_server, create_server};
pub use self::service::{BtpOutgoingService, BtpService};

pub trait BtpAccount: Account {
    fn get_btp_uri(&self) -> Option<&Url>;
}

/// The interface for Store implementations that can be used with the BTP Server.
pub trait BtpStore {
    type Account: BtpAccount;

    /// Load Account details based on the auth token received via BTP.
    fn get_account_from_btp_token(
        &self,
        token: &str,
        username: Option<&str>,
    ) -> Box<Future<Item = Self::Account, Error = ()> + Send>;
}

pub struct BtpOpenSignupAccount<'a> {
    pub auth_token: &'a str,
    pub username: Option<&'a str>,
    pub ilp_address: &'a [u8],
    pub asset_code: &'a str,
    pub asset_scale: u8,
}

/// The interface for Store implementatoins that allow open BTP signups.
/// Every incoming WebSocket connection will automatically have a BtpOpenSignupAccount
/// created and added to the store.
///
/// **WARNING:** Users and store implementors should be careful when implementing this trait because
/// malicious users can use open signups to create very large numbers of accounts and
/// crash the process or fill up the database.
pub trait BtpOpenSignupStore {
    type Account: BtpAccount;

    fn create_btp_account<'a>(
        &self,
        account: BtpOpenSignupAccount<'a>,
    ) -> Box<Future<Item = Self::Account, Error = ()> + Send>;
}

#[cfg(test)]
mod client_server {
    use super::*;
    use futures::future::{err, ok, result};
    use interledger_packet::{ErrorCode, FulfillBuilder, PrepareBuilder, RejectBuilder};
    use interledger_service::*;
    use std::{
        sync::Arc,
        time::{Duration, SystemTime},
    };
    use tokio::runtime::Runtime;

    #[derive(Clone, Debug)]
    pub struct TestAccount {
        pub id: u64,
        pub btp_incoming_token: Option<String>,
        pub btp_uri: Option<Url>,
    }

    impl Account for TestAccount {
        type AccountId = u64;

        fn id(&self) -> u64 {
            self.id
        }
    }

    impl BtpAccount for TestAccount {
        fn get_btp_uri(&self) -> Option<&Url> {
            self.btp_uri.as_ref()
        }
    }

    #[derive(Clone)]
    pub struct TestStore {
        accounts: Arc<Vec<TestAccount>>,
    }

    impl AccountStore for TestStore {
        type Account = TestAccount;

        fn get_accounts(
            &self,
            account_ids: Vec<<<Self as AccountStore>::Account as Account>::AccountId>,
        ) -> Box<Future<Item = Vec<Self::Account>, Error = ()> + Send> {
            let accounts: Vec<TestAccount> = self
                .accounts
                .iter()
                .filter_map(|account| {
                    if account_ids.contains(&account.id) {
                        Some(account.clone())
                    } else {
                        None
                    }
                })
                .collect();
            if accounts.len() == account_ids.len() {
                Box::new(ok(accounts))
            } else {
                Box::new(err(()))
            }
        }
    }

    impl BtpStore for TestStore {
        type Account = TestAccount;

        fn get_account_from_btp_token(
            &self,
            token: &str,
            _username: Option<&str>,
        ) -> Box<Future<Item = Self::Account, Error = ()> + Send> {
            Box::new(result(
                self.accounts
                    .iter()
                    .find(|account| {
                        if let Some(account_token) = &account.btp_incoming_token {
                            account_token == token
                        } else {
                            false
                        }
                    })
                    .cloned()
                    .ok_or(()),
            ))
        }
    }

    #[test]
    fn client_server_test() {
        let mut runtime = Runtime::new().unwrap();

        let server_store = TestStore {
            accounts: Arc::new(vec![TestAccount {
                id: 0,
                btp_incoming_token: Some("test_auth_token".to_string()),
                btp_uri: None,
            }]),
        };
        let server = create_server(
            "127.0.0.1:12345".parse().unwrap(),
            server_store,
            outgoing_service_fn(|_| RejectBuilder::new(ErrorCode::F02_UNREACHABLE)),
        )
        .and_then(|btp_server| {
            btp_server.handle_incoming(incoming_service_fn(|_| FulfillBuilder {
                fulfillment: &[0; 32],
                data: b"test data",
            }));
            Ok(())
        });
        runtime.spawn(server);

        let account = TestAccount {
            id: 0,
            btp_uri: Some(Url::parse("btp+ws://:test_auth_token@127.0.0.1:12345").unwrap()),
            btp_incoming_token: None,
        };
        let client_store = TestStore {
            accounts: Arc::new(vec![account.clone()]),
        };
        let accounts: Vec<u64> = vec![0];
        let client = connect_client(
            incoming_service_fn(|_| RejectBuilder::new(ErrorCode::F02_UNREACHABLE)),
            outgoing_service_fn(|_| RejectBuilder::new(ErrorCode::F02_UNREACHABLE)),
            client_store,
            accounts,
        )
        .and_then(move |mut btp_service| {
            let btp_service_clone = btp_service.clone();
            btp_service
                .send_request(OutgoingRequest {
                    from: account.clone(),
                    to: account.clone(),
                    prepare: PrepareBuilder {
                        destination: b"example.destination",
                        amount: 100,
                        execution_condition: &[0; 32],
                        expires_at: SystemTime::now() + Duration::from_secs(30),
                        data: b"test data",
                    }
                    .build(),
                })
                .map_err(|reject| println!("Packet was rejected: {:?}", reject))
                .and_then(move |_| {
                    btp_service_clone.close();
                    Ok(())
                })
        });
        runtime.block_on(client).unwrap();
    }
}