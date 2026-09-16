// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lore_base::types::Context;
use lore_proto::lore::notification::PublishRequest;
use lore_revision::lore::RepositoryId;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::USER_ID;
use tokio_stream::Stream;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::instrument;

use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::get_user_id;
use crate::grpc::get_verified_token;
use crate::grpc::no_repository_access_status;
use crate::grpc::timeout_grpc;

#[derive(Clone)]
pub struct NotificationService {
    sender: Arc<crate::notification::local::NotificationSender>,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    /// Bound on the subscribe authorization check, the request-handler
    /// timeout: a stalled online authorizer must not park subscribers.
    authorization_timeout: Duration,
}

impl NotificationService {
    pub fn new(
        sender: Arc<crate::notification::local::NotificationSender>,
        authorizer: Arc<dyn RepositoryAuthorizer>,
        authorization_timeout: Duration,
    ) -> Self {
        Self {
            sender,
            authorizer,
            authorization_timeout,
        }
    }
}

type SubscribeResponseStream =
    Pin<Box<dyn Stream<Item = Result<lore_proto::lore::notification::Event, Status>> + Send>>;

#[async_trait]
impl lore_notification::NotificationService for NotificationService {
    type SubscribeStream = SubscribeResponseStream;

    #[instrument(name = "NotificationService::Subscribe", skip_all)]
    async fn subscribe(
        &self,
        request: Request<lore_proto::lore::notification::SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let (_, extensions, message) = request.into_parts();
        let user_id = get_user_id(&extensions);
        let repository: RepositoryId = Context::from(message.repository).into();

        if repository.is_zero() {
            return Err(Status::failed_precondition("invalid stream"));
        }

        // Checked against the partition in the request *body*. The
        // default authorization middleware validates access using the
        // request metadata fields. The services passing a repository
        // in the body need custom verification logic. Denials are flattened
        // inside the timed check, so the only error escaping `timeout_grpc`
        // is the timeout's own status.
        let permitted = timeout_grpc(self.authorization_timeout, async {
            Ok(self
                .authorizer
                .check_repository_access(get_verified_token(&extensions).as_ref(), repository, None)
                .await
                .is_ok())
        })
        .await?;
        if !permitted {
            return Err(no_repository_access_status());
        }

        let rx = self.sender.register(repository);

        debug!(
            { REPOSITORY_ID } = %repository,
            { USER_ID } = user_id,
            "User subscribed to notifications"
        );

        let stream = BroadcastStream::new(rx).filter_map(|res| {
            match res {
                Ok(item) => Some(Ok(item)),
                // Ignore if client is lagging behind, just drop the event
                Err(BroadcastStreamRecvError::Lagged(_)) => None,
            }
        });

        Ok(Response::new(Box::pin(stream) as Self::SubscribeStream))
    }

    async fn publish(&self, _request: Request<PublishRequest>) -> Result<Response<()>, Status> {
        Err(Status::permission_denied(
            "Publish is not supported by the local notification service",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use lore_notification::NotificationService as _;
    use lore_proto::lore::notification::SubscribeRequest;
    use lore_transport::grpc::PARTITION_ID_KEY;
    use tonic::Code;

    use super::*;
    use crate::auth::jwt::AuthorizationToken;
    use crate::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
    use crate::authnz::repository_authorizer::RawToken;
    use crate::authnz::repository_authorizer::VerifiedToken;

    /// Denies everything and records which partition it was asked about.
    struct DenyingAuthorizer {
        asked: Mutex<Vec<RepositoryId>>,
    }

    #[async_trait]
    impl RepositoryAuthorizer for DenyingAuthorizer {
        async fn check_repository_access(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            repository_id: RepositoryId,
            _action: Option<&str>,
        ) -> Result<(), Status> {
            self.asked.lock().unwrap().push(repository_id);
            Err(Status::permission_denied("no grant"))
        }
    }

    fn repository(id: u8) -> RepositoryId {
        let mut data = [0u8; 16];
        data[15] = id;
        Context::from(data).into()
    }

    /// A subscribe whose *metadata* names one partition and whose *body*
    /// names another: only the body value — the one the stream registers
    /// on — may decide, so the check must be asked about it.
    #[tokio::test]
    async fn subscribe_is_decided_by_the_body_partition_not_the_metadata() {
        let authorizer = Arc::new(DenyingAuthorizer {
            asked: Mutex::new(Vec::new()),
        });
        let service = NotificationService::new(
            Arc::new(crate::notification::local::NotificationSender::default()),
            authorizer.clone(),
            Duration::from_secs(5),
        );

        let body_partition = repository(1);
        let metadata_partition = repository(2);
        let mut request = Request::new(SubscribeRequest {
            repository: Context::from(body_partition).into(),
        });
        request.metadata_mut().insert_bin(
            PARTITION_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(metadata_partition.data()),
        );
        request
            .extensions_mut()
            .insert(AuthorizationToken::default());
        request.extensions_mut().insert(RawToken("raw.jwt".into()));

        let denied = match service.subscribe(request).await {
            Err(status) => status,
            Ok(_) => panic!("an ungranted subscribe must be denied"),
        };

        assert_eq!(denied.code(), Code::PermissionDenied);
        assert_eq!(*authorizer.asked.lock().unwrap(), vec![body_partition]);
    }

    /// A no-auth configuration selects `AllowAllRepositoryAuthorizer`, under
    /// which subscribe stays open.
    #[tokio::test]
    async fn subscribe_stays_open_under_the_allow_all_authorizer() {
        let service = NotificationService::new(
            Arc::new(crate::notification::local::NotificationSender::default()),
            Arc::new(AllowAllRepositoryAuthorizer),
            Duration::from_secs(5),
        );
        let request = Request::new(SubscribeRequest {
            repository: Context::from(repository(1)).into(),
        });

        service
            .subscribe(request)
            .await
            .expect("a no-auth server's subscribe is open");
    }
}
