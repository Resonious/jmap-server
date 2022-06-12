use std::sync::Arc;

use crate::{
    orm::serialize::JMAPOrm,
    principal::schema::{Principal, Property, Type, Value},
    scrypt::{
        password_hash::{PasswordHash, PasswordVerifier},
        Scrypt,
    },
    types::jmap::JMAPId,
    SUPERUSER_ID,
};
use store::{
    core::{acl::ACLToken, collection::Collection, error::StoreError, JMAPIdPrefix},
    read::{
        comparator::Comparator,
        filter::{Filter, Query},
        FilterMapper,
    },
    tracing::debug,
    AccountId, JMAPStore, Store,
};

pub trait JMAPAccountStore {
    fn find_individual(&self, email: &str) -> store::Result<Option<AccountId>>;
    fn authenticate(&self, login: &str, password: &str) -> store::Result<Option<AccountId>>;
    fn get_acl_token(&self, primary_id: AccountId) -> store::Result<Arc<ACLToken>>;
    fn get_account_details(
        &self,
        account_id: AccountId,
    ) -> store::Result<Option<(String, String, Type)>>;
}

impl<T> JMAPAccountStore for JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn find_individual(&self, email: &str) -> store::Result<Option<AccountId>> {
        Ok(self
            .query_store::<FilterMapper>(
                SUPERUSER_ID,
                Collection::Principal,
                Filter::and(vec![
                    Filter::eq(Property::Email.into(), Query::Index(email.to_string())),
                    Filter::eq(Property::Type.into(), Query::Keyword("i".to_string())),
                ]),
                Comparator::None,
            )?
            .into_iter()
            .next()
            .map(|id| id.get_document_id()))
    }

    fn authenticate(&self, login: &str, password: &str) -> store::Result<Option<AccountId>> {
        if let Some(account_id) = self.find_individual(login)? {
            if let Some(mut fields) = self.get_orm::<Principal>(SUPERUSER_ID, account_id)? {
                if !matches!(
                    fields.get(&Property::Type),
                    Some(Value::Type {
                        value: Type::Individual
                    })
                ) {
                    debug!("Account {} is not an individual", JMAPId::from(account_id));
                    return Ok(None);
                }
                if let (
                    Some(Value::Text { value: email }),
                    Some(Value::Text {
                        value: password_hash,
                    }),
                ) = (
                    fields.remove(&Property::Email),
                    fields.remove(&Property::Secret),
                ) {
                    if email != login {
                        debug!(
                            "Login failed: Account {} has email {} but {} was used.",
                            JMAPId::from(account_id),
                            email,
                            login
                        );
                        return Ok(None);
                    }

                    if let Ok(password_hash) = PasswordHash::new(&password_hash) {
                        if Scrypt
                            .verify_password(password.as_bytes(), &password_hash)
                            .is_ok()
                        {
                            Ok(Some(account_id))
                        } else {
                            debug!(
                                "Login failed: Invalid password for account {}.",
                                JMAPId::from(account_id)
                            );
                            Ok(None)
                        }
                    } else {
                        debug!(
                            "Login failed: Account {} has an invalid password hash.",
                            JMAPId::from(account_id)
                        );
                        Ok(None)
                    }
                } else {
                    debug!(
                        "Account {} has no email or secret",
                        JMAPId::from(account_id)
                    );
                    Ok(None)
                }
            } else {
                debug!(
                    "Login failed: ORM for account {} does not exist.",
                    JMAPId::from(account_id)
                );
                Ok(None)
            }
        } else {
            debug!("Login failed: Login '{}' not found.", login);
            Ok(None)
        }
    }

    fn get_acl_token(&self, primary_id: AccountId) -> store::Result<Arc<ACLToken>> {
        self.acl_tokens
            .try_get_with::<_, StoreError>(primary_id, || {
                // Find all groups this account is a member of
                let mut member_of = vec![primary_id];
                let mut iter_stack = Vec::new();
                let mut current_id = primary_id;

                'outer: loop {
                    let mut iter = self
                        .query_store::<FilterMapper>(
                            SUPERUSER_ID,
                            Collection::Principal,
                            Filter::and(vec![
                                Filter::eq(Property::Members.into(), Query::Integer(current_id)),
                                Filter::eq(Property::Type.into(), Query::Keyword("g".to_string())),
                            ]),
                            Comparator::None,
                        )?
                        .into_iter()
                        .map(|id| id.get_document_id())
                        .collect::<Vec<_>>()
                        .into_iter();

                    loop {
                        while let Some(member_account) = iter.next() {
                            if !member_of.contains(&member_account) {
                                member_of.push(member_account);
                                if iter_stack.len() < 10 {
                                    iter_stack.push(iter);
                                    current_id = member_account;
                                    continue 'outer;
                                }
                            }
                        }

                        if let Some(prev_it) = iter_stack.pop() {
                            iter = prev_it;
                        } else {
                            break 'outer;
                        }
                    }
                }

                let access_to = self.get_shared_accounts(&member_of)?;

                Ok(ACLToken {
                    member_of,
                    access_to,
                }
                .into())
            })
            .map_err(|e| e.as_ref().clone())
    }

    fn get_account_details(
        &self,
        account_id: AccountId,
    ) -> store::Result<Option<(String, String, Type)>> {
        if let Some(mut fields) = self.get_orm::<Principal>(SUPERUSER_ID, account_id)? {
            Ok(Some((
                fields
                    .remove(&Property::Email)
                    .and_then(|v| {
                        if let Value::Text { value } = v {
                            Some(value)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default(),
                fields
                    .remove(&Property::Name)
                    .and_then(|v| {
                        if let Value::Text { value } = v {
                            Some(value)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default(),
                fields
                    .remove(&Property::Type)
                    .and_then(|v| {
                        if let Value::Type { value } = v {
                            Some(value)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(Type::Individual),
            )))
        } else {
            debug!(
                "Account details failed: ORM for account {} does not exist.",
                JMAPId::from(account_id)
            );
            Ok(None)
        }
    }
}
