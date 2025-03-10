use anyhow::Error;
use async_trait::async_trait;
use trust_dns_resolver::{
    error::ResolveError, lookup::Lookup, proto::rr::RecordType, TokioAsyncResolver,
};

#[async_trait]
pub trait Resolve: Sync + Send {
    async fn lookup(&self, name: &str, record_type: RecordType) -> Result<Lookup, ResolveError>;
}

#[derive(Clone)]
pub struct Resolver(pub TokioAsyncResolver);

#[async_trait]
impl Resolve for Resolver {
    async fn lookup(&self, name: &str, record_type: RecordType) -> Result<Lookup, ResolveError> {
        self.0.lookup(name, record_type).await
    }
}

#[derive(Debug, PartialEq)]
pub enum Record {
    Txt(String),
}

#[async_trait]
pub trait Create: Sync + Send {
    async fn create(&self, zone: &str, name: &str, record: Record) -> Result<(), Error>;
}

#[async_trait]
pub trait Delete: Sync + Send {
    async fn delete(&self, zone: &str, name: &str) -> Result<(), Error>;
}
