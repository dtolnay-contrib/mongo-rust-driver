mod common;
pub(crate) mod session;

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::{future::BoxFuture, Stream};
use serde::de::DeserializeOwned;

use crate::{
    bson::{from_document, Document},
    error::{Error, Result},
    operation::GetMore,
    results::GetMoreResult,
    Client,
    ClientSession,
    RUNTIME,
};
pub(crate) use common::{CursorInformation, CursorSpecification};
use common::{GenericCursor, GetMoreProvider, GetMoreProviderResult};

/// A [`Cursor`] streams the result of a query. When a query is made, the returned [`Cursor`] will
/// contain the first batch of results from the server; the individual results will then be returned
/// as the [`Cursor`] is iterated. When the batch is exhausted and if there are more results, the
/// [`Cursor`] will fetch the next batch of documents, and so forth until the results are exhausted.
/// Note that because of this batching, additional network I/O may occur on any given call to
/// `next`. Because of this, a [`Cursor`] iterates over `Result<T>` items rather than
/// simply `T` items.
///
/// The batch size of the `Cursor` can be configured using the options to the method that returns
/// it. For example, setting the `batch_size` field of
/// [`FindOptions`](options/struct.FindOptions.html) will set the batch size of the
/// `Cursor` returned by [`Collection::find`](struct.Collection.html#method.find).
///
/// Note that the batch size determines both the number of documents stored in memory by the
/// `Cursor` at a given time as well as the total number of network round-trips needed to fetch all
/// results from the server; both of these factors should be taken into account when choosing the
/// optimal batch size.
///
/// [`Cursor`] implements [`Stream`](https://docs.rs/futures/latest/futures/stream/trait.Stream.html), which means
/// it can be iterated over much in the same way that an `Iterator` can be in synchronous Rust. In
/// order to do so, the [`StreamExt`](https://docs.rs/futures/latest/futures/stream/trait.StreamExt.html) trait must
/// be imported. Because a [`Cursor`] iterates over a `Result<T>`, it also has access to the
/// potentially more ergonomic functionality provided by
/// [`TryStreamExt`](https://docs.rs/futures/latest/futures/stream/trait.TryStreamExt.html), which can be
/// imported instead of or in addition to
/// [`StreamExt`](https://docs.rs/futures/latest/futures/stream/trait.StreamExt.html). The methods from
/// [`TryStreamExt`](https://docs.rs/futures/latest/futures/stream/trait.TryStreamExt.html) are especially useful when
/// used in conjunction with the `?` operator.
///
/// ```rust
/// # use mongodb::{bson::Document, Client, error::Result};
/// #
/// # async fn do_stuff() -> Result<()> {
/// # let client = Client::with_uri_str("mongodb://example.com").await?;
/// # let coll = client.database("foo").collection::<Document>("bar");
/// #
/// use futures::stream::{StreamExt, TryStreamExt};
///
/// let mut cursor = coll.find(None, None).await?;
/// // regular Stream uses next() and iterates over Option<Result<T>>
/// while let Some(doc) = cursor.next().await {
///   println!("{}", doc?)
/// }
/// // regular Stream uses collect() and collects into a Vec<Result<T>>
/// let v: Vec<Result<_>> = cursor.collect().await;
///
/// let mut cursor = coll.find(None, None).await?;
/// // TryStream uses try_next() and iterates over Result<Option<T>>
/// while let Some(doc) = cursor.try_next().await? {
///   println!("{}", doc)
/// }
/// // TryStream uses try_collect() and collects into a Result<Vec<T>>
/// let v: Vec<_> = cursor.try_collect().await?;
/// #
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Cursor<T>
where
    T: DeserializeOwned + Unpin,
{
    client: Client,
    wrapped_cursor: ImplicitSessionCursor,
    _phantom: std::marker::PhantomData<T>,
}

impl<T> Cursor<T>
where
    T: DeserializeOwned + Unpin,
{
    pub(crate) fn new(
        client: Client,
        spec: CursorSpecification,
        session: Option<ClientSession>,
    ) -> Self {
        let provider = ImplicitSessionGetMoreProvider::new(&spec, session);

        Self {
            client: client.clone(),
            wrapped_cursor: ImplicitSessionCursor::new(client, spec, provider),
            _phantom: Default::default(),
        }
    }
}

impl<T> Stream for Cursor<T>
where
    T: DeserializeOwned + Unpin,
{
    type Item = Result<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = Pin::new(&mut self.wrapped_cursor).poll_next(cx);
        match next {
            Poll::Ready(opt) => Poll::Ready(
                opt.map(|result| result.and_then(|doc| from_document(doc).map_err(Into::into))),
            ),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> Drop for Cursor<T>
where
    T: DeserializeOwned + Unpin,
{
    fn drop(&mut self) {
        if self.wrapped_cursor.is_exhausted() {
            return;
        }

        let ns = self.wrapped_cursor.namespace();
        let coll = self
            .client
            .database(ns.db.as_str())
            .collection::<Document>(ns.coll.as_str());
        let cursor_id = self.wrapped_cursor.id();
        RUNTIME.execute(async move { coll.kill_cursor(cursor_id).await });
    }
}

/// A `GenericCursor` that optionally owns its own sessions.
/// This is to be used by cursors associated with implicit sessions.
type ImplicitSessionCursor = GenericCursor<ImplicitSessionGetMoreProvider>;

struct ImplicitSessionGetMoreResult {
    get_more_result: Result<GetMoreResult>,
    session: Option<Box<ClientSession>>,
}

impl GetMoreProviderResult for ImplicitSessionGetMoreResult {
    type Session = Option<Box<ClientSession>>;

    fn as_ref(&self) -> std::result::Result<&GetMoreResult, &Error> {
        self.get_more_result.as_ref()
    }

    fn into_parts(self) -> (Result<GetMoreResult>, Self::Session) {
        (self.get_more_result, self.session)
    }
}

/// A `GetMoreProvider` that optionally owns its own session.
/// This is to be used with cursors associated with implicit sessions.
enum ImplicitSessionGetMoreProvider {
    Executing(BoxFuture<'static, ImplicitSessionGetMoreResult>),
    Idle(Option<Box<ClientSession>>),
    Done,
}

impl ImplicitSessionGetMoreProvider {
    fn new(spec: &CursorSpecification, session: Option<ClientSession>) -> Self {
        if spec.id() == 0 {
            Self::Done
        } else {
            Self::Idle(session.map(Box::new))
        }
    }
}

impl GetMoreProvider for ImplicitSessionGetMoreProvider {
    type GetMoreResult = ImplicitSessionGetMoreResult;
    type GetMoreFuture = BoxFuture<'static, ImplicitSessionGetMoreResult>;

    fn executing_future(&mut self) -> Option<&mut Self::GetMoreFuture> {
        match self {
            Self::Executing(ref mut future) => Some(future),
            Self::Idle(_) | Self::Done => None,
        }
    }

    fn clear_execution(&mut self, session: Option<Box<ClientSession>>, exhausted: bool) {
        // If cursor is exhausted, immediately return implicit session to the pool.
        if exhausted {
            *self = Self::Done;
        } else {
            *self = Self::Idle(session)
        }
    }

    fn start_execution(&mut self, info: CursorInformation, client: Client) {
        take_mut::take(self, |self_| match self_ {
            Self::Idle(mut session) => {
                let future = Box::pin(async move {
                    let get_more = GetMore::new(info);
                    let get_more_result = client
                        .execute_operation(get_more, session.as_mut().map(|b| b.as_mut()))
                        .await;
                    ImplicitSessionGetMoreResult {
                        get_more_result,
                        session,
                    }
                });
                Self::Executing(future)
            }
            Self::Executing(_) | Self::Done => self_,
        })
    }
}
