use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use directories::ProjectDirs;
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRequest {
    pub file_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileResponse {
    pub file_data: Vec<u8>,
    pub file_name: String,
    pub file_size: u64,
}

// Simplified file transfer service without complex libp2p request-response
// This provides basic file storage and retrieval functionality

#[derive(Debug)]
pub enum FileTransferCommand {
    UploadFile {
        file_path: String,
        file_name: String,
        respond_to: oneshot::Sender<Result<String, String>>,
    },
    DownloadFile {
        file_hash: String,
        output_path: String,
        respond_to: oneshot::Sender<Result<(), String>>,
    },
    GetStoredFiles,
}

#[derive(Debug, Clone)]
pub enum FileTransferEvent {
    FileUploaded {
        file_hash: String,
        file_name: String,
    },
    FileDownloaded {
        file_path: String,
    },
    FileNotFound {
        file_hash: String,
    },
    Error {
        message: String,
    },
}

const CHUNK_SIZE: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkMetadata {
    pub index: u32,
    pub hash: String,
    pub size: u64,
    pub stored_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionMetadata {
    pub algorithm: String,
    pub nonce_size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionMetadata {
    pub algorithm: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredFile {
    pub file_hash: String,
    pub file_name: String,
    pub file_size: u64,
    pub chunk_size: u64,
    pub chunks: Vec<ChunkMetadata>,
    pub encryption: Option<EncryptionMetadata>,
    pub compression: Option<CompressionMetadata>,
    pub created_at: u64,
}

#[derive(Debug, Clone)]
struct StorageConfig {
    encrypt_chunks: bool,
    compression: bool,
    encryption_key: Option<[u8; 32]>,
}

impl StorageConfig {
    fn encryption_metadata(&self) -> Option<EncryptionMetadata> {
        if self.encrypt_chunks {
            Some(EncryptionMetadata {
                algorithm: "AES-256-GCM".to_string(),
                nonce_size: 12,
            })
        } else {
            None
        }
    }

    fn compression_metadata(&self) -> Option<CompressionMetadata> {
        if self.compression {
            Some(CompressionMetadata {
                algorithm: "zlib".to_string(),
            })
        } else {
            None
        }
    }
}

pub struct FileTransferService {
    cmd_tx: mpsc::Sender<FileTransferCommand>,
    event_rx: Arc<Mutex<mpsc::Receiver<FileTransferEvent>>>,
    state: Arc<FileTransferState>,
}

#[derive(Debug)]
struct FileTransferState {
    stored_files: Mutex<HashMap<String, StoredFile>>,
    storage_root: PathBuf,
    chunks_dir: PathBuf,
    manifests_dir: PathBuf,
    config: StorageConfig,
}

impl FileTransferState {
    async fn store_from_path(&self, file_path: &Path, file_name: &str) -> Result<String, String> {
        let mut file = fs::File::open(file_path)
            .await
            .map_err(|e| format!("Failed to read file: {}", e))?;
        let mut buffer = vec![0u8; CHUNK_SIZE];
        let mut hasher = Sha256::new();
        let mut total_size = 0u64;
        let mut chunk_index: u32 = 0;
        let mut chunks = Vec::new();

        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|e| format!("Failed to read file chunk: {}", e))?;
            if read == 0 {
                break;
            }

            let chunk_data = &buffer[..read];
            total_size += read as u64;
            hasher.update(chunk_data);
            let chunk_hash = Self::hash_chunk(chunk_data);
            let stored_size = self.persist_chunk(&chunk_hash, chunk_data).await?;
            chunks.push(ChunkMetadata {
                index: chunk_index,
                hash: chunk_hash,
                size: read as u64,
                stored_size,
            });
            chunk_index += 1;
        }

        let file_hash = format!("{:x}", hasher.finalize());
        self.persist_manifest(file_hash, file_name, total_size, chunks)
            .await
    }

    async fn store_from_bytes(&self, file_name: &str, data: &[u8]) -> Result<String, String> {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let file_hash = format!("{:x}", hasher.finalize());

        let mut chunks = Vec::new();
        let mut chunk_index: u32 = 0;
        let mut offset = 0usize;

        while offset < data.len() {
            let end = (offset + CHUNK_SIZE).min(data.len());
            let chunk_data = &data[offset..end];
            let chunk_hash = Self::hash_chunk(chunk_data);
            let stored_size = self.persist_chunk(&chunk_hash, chunk_data).await?;
            chunks.push(ChunkMetadata {
                index: chunk_index,
                hash: chunk_hash,
                size: chunk_data.len() as u64,
                stored_size,
            });
            chunk_index += 1;
            offset = end;
        }

        self.persist_manifest(file_hash, file_name, data.len() as u64, chunks)
            .await
    }

    async fn persist_manifest(
        &self,
        file_hash: String,
        file_name: &str,
        file_size: u64,
        chunks: Vec<ChunkMetadata>,
    ) -> Result<String, String> {
        let manifest = StoredFile {
            file_hash: file_hash.clone(),
            file_name: file_name.to_string(),
            file_size,
            chunk_size: CHUNK_SIZE as u64,
            chunks,
            encryption: self.config.encryption_metadata(),
            compression: self.config.compression_metadata(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| format!("Failed to compute timestamp: {}", e))?
                .as_secs(),
        };

        let manifest_path = self
            .manifests_dir
            .join(format!("{}.json", manifest.file_hash));
        let payload = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| format!("Failed to serialize manifest: {}", e))?;
        fs::write(&manifest_path, payload)
            .await
            .map_err(|e| format!("Failed to write manifest: {}", e))?;

        let mut guard = self.stored_files.lock().await;
        guard.insert(manifest.file_hash.clone(), manifest);

        Ok(file_hash)
    }

    async fn persist_chunk(&self, chunk_hash: &str, data: &[u8]) -> Result<u64, String> {
        let chunk_path = self.chunks_dir.join(chunk_hash);
        if let Ok(metadata) = fs::metadata(&chunk_path).await {
            return Ok(metadata.len());
        }

        let encoded = self.encode_chunk(data)?;
        fs::write(&chunk_path, &encoded)
            .await
            .map_err(|e| format!("Failed to write chunk {}: {}", chunk_hash, e))?;
        Ok(encoded.len() as u64)
    }

    fn encode_chunk(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        let mut payload = if self.config.compression {
            self.compress_chunk(data)?
        } else {
            data.to_vec()
        };

        if self.config.encrypt_chunks {
            let key = self
                .config
                .encryption_key
                .as_ref()
                .ok_or_else(|| "Encryption key not configured".to_string())?;
            let cipher = Aes256Gcm::new(Key::from_slice(key));
            let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
            let mut encrypted = cipher
                .encrypt(&nonce, payload.as_ref())
                .map_err(|e| format!("Chunk encryption failed: {}", e))?;
            let mut combined = nonce.to_vec();
            combined.append(&mut encrypted);
            Ok(combined)
        } else {
            Ok(payload)
        }
    }

    fn decode_chunk(
        &self,
        data: Vec<u8>,
        encrypted: bool,
        compressed: bool,
    ) -> Result<Vec<u8>, String> {
        let mut payload = data;

        if encrypted {
            let key = self
                .config
                .encryption_key
                .as_ref()
                .ok_or_else(|| "Encryption key not configured".to_string())?;
            if payload.len() < 12 {
                return Err("Encrypted chunk is too small to contain a nonce".to_string());
            }
            let (nonce_bytes, ciphertext) = payload.split_at(12);
            let cipher = Aes256Gcm::new(Key::from_slice(key));
            payload = cipher
                .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
                .map_err(|e| format!("Chunk decryption failed: {}", e))?;
        }

        if compressed {
            let mut decoder = ZlibDecoder::new(&payload[..]);
            let mut decompressed = Vec::new();
            decoder
                .read_to_end(&mut decompressed)
                .map_err(|e| format!("Chunk decompression failed: {}", e))?;
            Ok(decompressed)
        } else {
            Ok(payload)
        }
    }

    async fn assemble_file(&self, file_hash: &str, output_path: &Path) -> Result<(), String> {
        let manifest = {
            let guard = self.stored_files.lock().await;
            guard
                .get(file_hash)
                .cloned()
        }
        .ok_or_else(|| "File not found locally".to_string())?;

        let encrypted = manifest.encryption.is_some();
        let compressed = manifest.compression.is_some();

        let mut output = fs::File::create(output_path)
            .await
            .map_err(|e| format!("Failed to create {}: {}", output_path.display(), e))?;

        for chunk in manifest.chunks {
            let chunk_path = self.chunks_dir.join(&chunk.hash);
            let raw = fs::read(&chunk_path)
                .await
                .map_err(|e| format!("Failed to read chunk {}: {}", chunk.hash, e))?;
            let decoded = self.decode_chunk(raw, encrypted, compressed)?;
            if decoded.len() as u64 != chunk.size {
                return Err(format!(
                    "Chunk {} size mismatch (expected {}, got {})",
                    chunk.index,
                    chunk.size,
                    decoded.len()
                ));
            }
            output
                .write_all(&decoded)
                .await
                .map_err(|e| format!("Failed to write chunk {}: {}", chunk.index, e))?;
        }

        Ok(())
    }

    fn compress_chunk(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(data)
            .map_err(|e| format!("Compression failed: {}", e))?;
        encoder
            .finish()
            .map_err(|e| format!("Compression finalize failed: {}", e))
    }

    fn hash_chunk(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        format!("{:x}", hasher.finalize())
    }
}

impl FileTransferService {
    pub async fn new() -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel(100);
        let (event_tx, event_rx) = mpsc::channel(100);
        let project_dirs = ProjectDirs::from("com", "Chiral", "ChiralNetwork")
            .ok_or_else(|| "Unable to determine storage directory".to_string())?;

        let storage_root = PathBuf::from(project_dirs.data_dir());
        let chunks_dir = storage_root.join("chunks");
        let manifests_dir = storage_root.join("manifests");

        fs::create_dir_all(&chunks_dir)
            .await
            .map_err(|e| format!("Failed to create chunks directory: {}", e))?;
        fs::create_dir_all(&manifests_dir)
            .await
            .map_err(|e| format!("Failed to create manifests directory: {}", e))?;

        let stored_files_map = Self::load_existing_manifests(&manifests_dir).await?;

        let state = Arc::new(FileTransferState {
            stored_files: Mutex::new(stored_files_map),
            storage_root,
            chunks_dir,
            manifests_dir,
            config: StorageConfig {
                encrypt_chunks: false,
                compression: false,
                encryption_key: None,
            },
        });

        // Spawn the file transfer service task
        tokio::spawn(Self::run_file_transfer_service(cmd_rx, event_tx, state.clone()));

        Ok(FileTransferService {
            cmd_tx,
            event_rx: Arc::new(Mutex::new(event_rx)),
            state,
        })
    }

    async fn load_existing_manifests(manifests_dir: &Path) -> Result<HashMap<String, StoredFile>, String> {
        let mut stored_files = HashMap::new();

        let mut entries = match fs::read_dir(manifests_dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(stored_files),
            Err(e) => {
                return Err(format!(
                    "Failed to read manifests directory {}: {}",
                    manifests_dir.display(),
                    e
                ))
            }
        };

        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| format!("Failed to iterate manifests: {}", e))?
        {
            let path = entry.path();
            if entry
                .file_type()
                .await
                .map_err(|e| format!("Failed to inspect {:?}: {}", path, e))?
                .is_file()
            {
                match fs::read(&path).await {
                    Ok(bytes) => match serde_json::from_slice::<StoredFile>(&bytes) {
                        Ok(manifest) => {
                            stored_files.insert(manifest.file_hash.clone(), manifest);
                        }
                        Err(e) => {
                            warn!("Ignoring corrupt manifest {:?}: {}", path, e);
                        }
                    },
                    Err(e) => {
                        warn!("Failed to load manifest {:?}: {}", path, e);
                    }
                }
            }
        }

        Ok(stored_files)
    }

    async fn run_file_transfer_service(
        mut cmd_rx: mpsc::Receiver<FileTransferCommand>,
        event_tx: mpsc::Sender<FileTransferEvent>,
        state: Arc<FileTransferState>,
    ) {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                FileTransferCommand::UploadFile {
                    file_path,
                    file_name,
                    respond_to,
                } => match Self::handle_upload_file(&file_path, &file_name, state.clone()).await {
                    Ok(file_hash) => {
                        let _ = respond_to.send(Ok(file_hash.clone()));
                        let _ = event_tx
                            .send(FileTransferEvent::FileUploaded {
                                file_hash: file_hash.clone(),
                                file_name: file_name.clone(),
                            })
                            .await;
                        info!("File uploaded successfully: {} -> {}", file_name, file_hash);
                    }
                    Err(e) => {
                        let _ = respond_to.send(Err(e.clone()));
                        let error_msg = format!("Upload failed: {}", e);
                        let _ = event_tx
                            .send(FileTransferEvent::Error {
                                message: error_msg.clone(),
                            })
                            .await;
                        error!("File upload failed: {}", error_msg);
                    }
                },
                FileTransferCommand::DownloadFile {
                    file_hash,
                    output_path,
                    respond_to,
                } => {
                    match Self::handle_download_file(&file_hash, &output_path, state.clone()).await {
                        Ok(()) => {
                            let _ = respond_to.send(Ok(()));
                            let _ = event_tx
                                .send(FileTransferEvent::FileDownloaded {
                                    file_path: output_path.clone(),
                                })
                                .await;
                            info!(
                                "File downloaded successfully: {} -> {}",
                                file_hash, output_path
                            );
                        }
                        Err(e) => {
                            let _ = respond_to.send(Err(e.clone()));
                            let error_msg = format!("Download failed: {}", e);
                            let _ = event_tx
                                .send(FileTransferEvent::Error {
                                    message: error_msg.clone(),
                                })
                                .await;
                            error!("File download failed: {}", error_msg);
                        }
                    }
                }
                FileTransferCommand::GetStoredFiles => {
                    // This could be used to list available files
                    debug!("GetStoredFiles command received");
                }
            }
        }
    }

    async fn handle_upload_file(
        file_path: &str,
        file_name: &str,
        state: Arc<FileTransferState>,
    ) -> Result<String, String> {
        state
            .store_from_path(Path::new(file_path), file_name)
            .await
    }

    async fn handle_download_file(
        file_hash: &str,
        output_path: &str,
        state: Arc<FileTransferState>,
    ) -> Result<(), String> {
        state
            .assemble_file(file_hash, Path::new(output_path))
            .await
    }

    pub fn calculate_file_hash(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        format!("{:x}", hasher.finalize())
    }

    pub async fn upload_file(&self, file_path: String, file_name: String) -> Result<String, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(FileTransferCommand::UploadFile {
                file_path,
                file_name,
                respond_to: tx,
            })
            .await
            .map_err(|e| e.to_string())?;
        rx.await.map_err(|e| e.to_string())?
    }

    pub async fn download_file(
        &self,
        file_hash: String,
        output_path: String,
    ) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(FileTransferCommand::DownloadFile {
                file_hash,
                output_path,
                respond_to: tx,
            })
            .await
            .map_err(|e| e.to_string())?;
        rx.await.map_err(|e| e.to_string())?
    }

    pub async fn get_stored_files(&self) -> Result<Vec<(String, String)>, String> {
        let files = self.state.stored_files.lock().await;
        Ok(files
            .iter()
            .map(|(hash, manifest)| (hash.clone(), manifest.file_name.clone()))
            .collect())
    }

    pub async fn drain_events(&self, max: usize) -> Vec<FileTransferEvent> {
        let mut events = Vec::new();
        let mut event_rx = self.event_rx.lock().await;

        for _ in 0..max {
            match event_rx.try_recv() {
                Ok(event) => events.push(event),
                Err(_) => break,
            }
        }

        events
    }

    pub async fn store_file_data(&self, file_name: String, file_data: Vec<u8>) -> Result<String, String> {
        self.state.store_from_bytes(&file_name, &file_data).await
    }
}
