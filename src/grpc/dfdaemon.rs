/*
 *     Copyright 2023 The Dragonfly Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::shutdown;
use crate::task;
use crate::Result as ClientResult;
use dragonfly_api::common::v2::{Piece, Task};
use dragonfly_api::dfdaemon::v2::{
    dfdaemon_client::DfdaemonClient as DfdaemonGRPCClient,
    dfdaemon_server::{Dfdaemon, DfdaemonServer as DfdaemonGRPCServer},
    sync_pieces_request, sync_pieces_response, DeleteTaskRequest, DownloadTaskRequest,
    DownloadTaskResponse, GetPieceNumbersRequest, GetPieceNumbersResponse, InterestedPiecesRequest,
    InterestedPiecesResponse, StatTaskRequest as DfdaemonStatTaskRequest, SyncPiecesRequest,
    SyncPiecesResponse, UploadTaskRequest,
};
use dragonfly_api::scheduler::v2::StatTaskRequest as SchedulerStatTaskRequest;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::codec::CompressionEncoding;
use tonic::{
    transport::{Channel, Endpoint, Server, Uri},
    Request, Response, Status,
};
use tower::service_fn;
use tracing::{error, info};

// DfdaemonServer is the grpc server of the dfdaemon.
pub struct DfdaemonServer {
    // addr is the address of the grpc server.
    addr: SocketAddr,

    // socket_path is the path of the unix domain socket.
    socket_path: PathBuf,

    // task is the task manager.
    task: Arc<task::Task>,

    // shutdown is used to shutdown the grpc server.
    shutdown: shutdown::Shutdown,

    // _shutdown_complete is used to notify the grpc server is shutdown.
    _shutdown_complete: mpsc::UnboundedSender<()>,
}

// DfdaemonServer implements the grpc server of the dfdaemon.
impl DfdaemonServer {
    // new creates a new DfdaemonServer.
    pub fn new(
        addr: SocketAddr,
        socket_path: PathBuf,
        task: Arc<task::Task>,
        shutdown: shutdown::Shutdown,
        shutdown_complete_tx: mpsc::UnboundedSender<()>,
    ) -> Self {
        Self {
            addr,
            socket_path,
            task,
            shutdown,
            _shutdown_complete: shutdown_complete_tx,
        }
    }

    // run starts the metrics server.
    pub async fn run(&self) {
        // Register the reflection service.
        let reflection = tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(dragonfly_api::FILE_DESCRIPTOR_SET)
            .build()
            .unwrap();

        // Initialize the grpc service.
        let service = DfdaemonGRPCServer::new(DfdaemonServerHandler {
            task: self.task.clone(),
        })
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(usize::MAX);

        // Clone the shutdown channel.
        let mut shutdown = self.shutdown.clone();

        // Start upload grpc server.
        info!("upload server listening on {}", self.addr);
        Server::builder()
            .add_service(reflection.clone())
            .add_service(service.clone())
            .serve_with_shutdown(self.addr, async move {
                // Upload grpc server shutting down with signals.
                let _ = shutdown.recv().await;
                info!("upload grpc server shutting down");
            })
            .await
            .unwrap();

        // Clone the shutdown channel.
        let mut shutdown_unix = self.shutdown.clone();

        // Start download grpc server with unix domain socket.
        info!(
            "download server listening on {}",
            self.socket_path.display()
        );
        let uds = UnixListener::bind(&self.socket_path).unwrap();
        let uds_stream = UnixListenerStream::new(uds);
        Server::builder()
            .add_service(reflection.clone())
            .add_service(service.clone())
            .serve_with_incoming_shutdown(uds_stream, async move {
                // Download grpc server shutting down with signals.
                let _ = shutdown_unix.recv().await;
                info!("download grpc server shutting down");
            })
            .await
            .unwrap();
    }
}

// DfdaemonServerHandler is the handler of the dfdaemon grpc service.
pub struct DfdaemonServerHandler {
    // task is the task manager.
    task: Arc<task::Task>,
}

// DfdaemonServerHandler implements the dfdaemon grpc service.
#[tonic::async_trait]
impl Dfdaemon for DfdaemonServerHandler {
    // get_piece_numbers gets the piece numbers.
    async fn get_piece_numbers(
        &self,
        request: Request<GetPieceNumbersRequest>,
    ) -> Result<Response<GetPieceNumbersResponse>, Status> {
        let request = request.into_inner();
        let task_id = request.task_id.clone();
        let task = self.task.clone();

        // Get the piece numbers from the local storage.
        let piece_numbers: Vec<i32> = task
            .piece
            .get_all(task_id.as_str())
            .map_err(|e| {
                error!("get piece numbers from local storage: {}", e);
                Status::internal(e.to_string())
            })?
            .iter()
            .map(|piece| piece.number)
            .collect();

        // Check whether the piece numbers is empty.
        if piece_numbers.is_empty() {
            error!("piece numbers not found");
            return Err(Status::not_found("piece numbers not found"));
        }

        Ok(Response::new(GetPieceNumbersResponse { piece_numbers }))
    }

    // SyncPiecesStream is the stream of the sync pieces response.
    type SyncPiecesStream = ReceiverStream<Result<SyncPiecesResponse, Status>>;

    // sync_pieces syncs the pieces.
    async fn sync_pieces(
        &self,
        request: Request<SyncPiecesRequest>,
    ) -> Result<Response<Self::SyncPiecesStream>, Status> {
        // Clone the request.
        let request = request.into_inner();

        // Clone the task.
        let task = self.task.clone();

        // Get the task id from the request.
        let task_id = request.task_id.clone();

        // Get the interested piece numbers from the request.
        let interested_piece_numbers = match request.request {
            Some(sync_pieces_request::Request::InterestedPiecesRequest(
                InterestedPiecesRequest { piece_numbers },
            )) => piece_numbers,
            _ => {
                error!("missing interested pieces request");
                return Err(Status::invalid_argument(
                    "missing interested pieces request",
                ));
            }
        };

        // Initialize stream channel.
        let (out_stream_tx, out_stream_rx) = mpsc::channel(128);
        tokio::spawn(async move {
            for interested_piece_number in interested_piece_numbers {
                // Get the piece metadata from the local storage.
                let piece = match task.piece.get(&task_id, interested_piece_number) {
                    Ok(piece) => piece,
                    Err(e) => {
                        error!("get piece metadata from local storage: {}", e);
                        continue;
                    }
                };

                // Check whether the piece exists.
                let piece = match piece {
                    Some(piece) => piece,
                    None => {
                        error!("piece {} not found", interested_piece_number);
                        continue;
                    }
                };

                // Get the piece content from the local storage.
                let mut reader = match task
                    .piece
                    .download_from_local_peer(&task_id, interested_piece_number)
                    .await
                {
                    Ok(reader) => reader,
                    Err(e) => {
                        error!("get piece content from local peer: {}", e);
                        continue;
                    }
                };

                // Read the content of the piece.
                let mut content = Vec::new();
                match reader.read_to_end(&mut content).await {
                    Ok(_) => {}
                    Err(e) => {
                        error!("read piece content: {}", e);
                        continue;
                    }
                };

                // Send the interested pieces response.
                out_stream_tx
                    .send(Ok(SyncPiecesResponse {
                        response: Some(sync_pieces_response::Response::InterestedPiecesResponse(
                            InterestedPiecesResponse {
                                piece: Some(Piece {
                                    number: piece.number,
                                    parent_id: None,
                                    offset: piece.offset,
                                    length: piece.length,
                                    digest: piece.digest,
                                    content: Some(content),
                                    traffic_type: None,
                                    cost: None,
                                    created_at: None,
                                }),
                            },
                        )),
                    }))
                    .await
                    .unwrap_or_else(|e| {
                        error!("send to out stream: {}", e);
                    });
            }
        });

        Ok(Response::new(ReceiverStream::new(out_stream_rx)))
    }

    // DownloadTaskStream is the stream of the download task response.
    type DownloadTaskStream = ReceiverStream<Result<DownloadTaskResponse, Status>>;

    // download_task tells the dfdaemon to download the task.
    async fn download_task(
        &self,
        request: Request<DownloadTaskRequest>,
    ) -> Result<Response<Self::DownloadTaskStream>, Status> {
        // Clone the request.
        let request = request.into_inner();

        // Check whether the download is empty.
        let download = request
            .download
            .ok_or(Status::invalid_argument("missing download"))?
            .clone();

        // Clone the task.
        let task = self.task.clone();

        // Initialize stream channel.
        let (out_stream_tx, out_stream_rx) = mpsc::channel(128);
        tokio::spawn(async move {
            match task.download_into_file(download).await {
                Ok(mut download_progress_rx) => {
                    while let Some(finished_piece) = download_progress_rx.recv().await {
                        out_stream_tx
                            .send(Ok(DownloadTaskResponse {
                                piece: Some(Piece {
                                    number: finished_piece.number,
                                    parent_id: None,
                                    offset: finished_piece.offset,
                                    length: finished_piece.length,
                                    digest: finished_piece.clone().digest,
                                    content: None,
                                    traffic_type: None,
                                    cost: finished_piece
                                        .cost()
                                        .map(prost_wkt_types::Duration::from),
                                    created_at: Some(prost_wkt_types::Timestamp::from(
                                        finished_piece.created_at,
                                    )),
                                }),
                            }))
                            .await
                            .unwrap_or_else(|e| {
                                error!("send to out stream: {}", e);
                            });
                    }
                }
                Err(e) => {
                    error!("download task: {}", e);
                    out_stream_tx
                        .send(Err(Status::internal(e.to_string())))
                        .await
                        .unwrap_or_else(|e| {
                            error!("send to out stream: {}", e);
                        });
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(out_stream_rx)))
    }

    // upload_task tells the dfdaemon to upload the task.
    async fn upload_task(
        &self,
        request: Request<UploadTaskRequest>,
    ) -> Result<Response<()>, Status> {
        println!("upload_task: {:?}", request);
        Err(Status::unimplemented("not implemented"))
    }

    // stat_task gets the status of the task.
    async fn stat_task(
        &self,
        request: Request<DfdaemonStatTaskRequest>,
    ) -> Result<Response<Task>, Status> {
        let mut request = tonic::Request::new(SchedulerStatTaskRequest {
            id: request.into_inner().task_id.clone(),
        });
        request.set_timeout(super::REQUEST_TIMEOUT);

        self.task
            .scheduler_client
            .client()
            .map_err(|e| Status::internal(e.to_string()))?
            .stat_task(request)
            .await
    }

    // delete_task tells the dfdaemon to delete the task.
    async fn delete_task(
        &self,
        request: Request<DeleteTaskRequest>,
    ) -> Result<Response<()>, Status> {
        println!("delete_task: {:?}", request);
        Err(Status::unimplemented("not implemented"))
    }
}

// DfdaemonClient is a wrapper of DfdaemonGRPCClient.
#[derive(Clone)]
pub struct DfdaemonClient {
    // client is the grpc client of the dfdaemon.
    pub client: DfdaemonGRPCClient<Channel>,
}

// DfdaemonClient implements the grpc client of the dfdaemon.
impl DfdaemonClient {
    // new creates a new DfdaemonClient.
    pub async fn new(addr: String) -> ClientResult<Self> {
        let channel = Channel::from_static(Box::leak(addr.into_boxed_str()))
            .connect()
            .await?;
        let client = DfdaemonGRPCClient::new(channel)
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(usize::MAX);
        Ok(Self { client })
    }

    // new_unix creates a new DfdaemonClient with unix domain socket.
    pub async fn new_unix(socket_path: PathBuf) -> ClientResult<Self> {
        // Ignore the uri because it is not used.
        let channel = Endpoint::try_from("http://[::]:50051")
            .unwrap()
            .connect_with_connector(service_fn(move |_: Uri| {
                UnixStream::connect(socket_path.clone())
            }))
            .await?;
        let client = DfdaemonGRPCClient::new(channel)
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(usize::MAX);
        Ok(Self { client })
    }

    // get_piece_numbers gets the piece numbers.
    pub async fn get_piece_numbers(
        &self,
        request: GetPieceNumbersRequest,
    ) -> ClientResult<Vec<i32>> {
        let mut request = tonic::Request::new(request);
        request.set_timeout(super::REQUEST_TIMEOUT);

        let response = self.client.clone().get_piece_numbers(request).await?;
        Ok(response.into_inner().piece_numbers)
    }

    // sync_pieces syncs the pieces.
    pub async fn sync_pieces(
        &self,
        request: SyncPiecesRequest,
    ) -> ClientResult<tonic::Response<tonic::codec::Streaming<SyncPiecesResponse>>> {
        let mut request = tonic::Request::new(request);
        request.set_timeout(super::REQUEST_TIMEOUT);

        let response = self.client.clone().sync_pieces(request).await?;
        Ok(response)
    }

    // download_task tells the dfdaemon to download the task.
    pub async fn download_task(
        &self,
        request: DownloadTaskRequest,
    ) -> ClientResult<tonic::Response<tonic::codec::Streaming<DownloadTaskResponse>>> {
        // Get the timeout from the request.
        let timeout = request
            .clone()
            .download
            .ok_or_else(|| {
                tonic::Status::invalid_argument("missing download in download task request")
            })?
            .timeout;

        // Initialize the request.
        let mut request = tonic::Request::new(request);

        // Set the timeout to the request.
        if let Some(timeout) = timeout {
            request.set_timeout(
                Duration::try_from(timeout)
                    .map_err(|_| tonic::Status::invalid_argument("invalid timeout"))?,
            );
        }

        let response = self.client.clone().download_task(request).await?;
        Ok(response)
    }

    // upload_task tells the dfdaemon to upload the task.
    pub async fn upload_task(&self, request: UploadTaskRequest) -> ClientResult<()> {
        let mut request = tonic::Request::new(request);
        request.set_timeout(super::REQUEST_TIMEOUT);

        self.client.clone().upload_task(request).await?;
        Ok(())
    }

    // stat_task gets the status of the task.
    pub async fn stat_task(&self, request: DfdaemonStatTaskRequest) -> ClientResult<Task> {
        let mut request = tonic::Request::new(request);
        request.set_timeout(super::REQUEST_TIMEOUT);

        let response = self.client.clone().stat_task(request).await?;
        Ok(response.into_inner())
    }

    // delete_task tells the dfdaemon to delete the task.
    pub async fn delete_task(&self, request: DeleteTaskRequest) -> ClientResult<()> {
        let mut request = tonic::Request::new(request);
        request.set_timeout(super::REQUEST_TIMEOUT);

        self.client.clone().delete_task(request).await?;
        Ok(())
    }
}
