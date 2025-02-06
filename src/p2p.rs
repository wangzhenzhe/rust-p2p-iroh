use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use iroh::{Endpoint, NodeAddr, PublicKey, SecretKey};
use tokio::sync::{mpsc, broadcast, RwLock};
use tracing::{info, warn, error};

const P2P_ALPN: &[u8] = b"P2P_PROTO_V1";

#[derive(Debug, Clone)]
pub struct Message {
    pub from: PublicKey,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
struct PeerConnection {
    #[allow(dead_code)]  // 显式标记此字段是有意保留的
    connection: iroh::endpoint::Connection,  // 保持连接活跃
    send_stream: Arc<tokio::sync::Mutex<iroh::endpoint::SendStream>>,
}

impl PeerConnection {
    // 添加一个方法来检查连接状态
    async fn is_alive(&self) -> bool {
        // 尝试发送一个心跳消息
        let mut send_guard = match self.send_stream.try_lock() {
            Ok(guard) => guard,
            Err(_) => return true, // 如果有其他任务正在使用send_stream，说明连接还活着
        };

        // 尝试发送一个空消息来检查连接
        match send_guard.write_all(&[]).await {
            Ok(_) => true,
            Err(_) => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct P2PNode {
    endpoint: Arc<Endpoint>,
    public_key: PublicKey,
    connections: Arc<RwLock<HashMap<PublicKey, PeerConnection>>>,
    message_tx: broadcast::Sender<Message>,
}

impl P2PNode {
    pub async fn new(secret_key: Option<SecretKey>) -> Result<Self> {
        let secret_key = secret_key.unwrap_or_else(|| SecretKey::generate(rand::rngs::OsRng));
        let public_key = secret_key.public();
        
        info!("创建新节点公钥: {}", public_key);
        
        let endpoint = Arc::new(
            Endpoint::builder()
                .secret_key(secret_key)
                .alpns(vec![P2P_ALPN.to_vec()])
                .bind_addr_v4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
                .bind()
                .await?
        );

        let (message_tx, _) = broadcast::channel(100);

        Ok(Self {
            endpoint,
            public_key,
            connections: Arc::new(RwLock::new(HashMap::new())),
            message_tx,
        })
    }

    pub async fn start_listening(&self) -> Result<()> {
        info!("等待中继节点连接...");
        self.wait_for_relay().await?;
        
        let addr = self.endpoint.node_addr().await?;
        info!("节点ID: {}", addr.node_id);
        info!("监听地址: {:#?}", addr);
        
        let connections = self.connections.clone();
        let message_tx = self.message_tx.clone();
        let endpoint = self.endpoint.clone();

        tokio::spawn(async move {
            info!("开始监听传入连接");
            while let Some(incoming) = endpoint.accept().await {
                info!("收到新的连接请求");
                let connections = connections.clone();
                let message_tx = message_tx.clone();
                
                tokio::spawn(async move {
                    if let Err(e) = Self::handle_incoming(incoming, connections, message_tx).await {
                        error!("处理连接时出错: {:?}", e);
                    }
                });
            }
        });

        Ok(())
    }

    async fn handle_incoming(
        incoming: iroh::endpoint::Incoming,
        connections: Arc<RwLock<HashMap<PublicKey, PeerConnection>>>,
        message_tx: broadcast::Sender<Message>,
    ) -> Result<()> {
        let connection = incoming.await?;
        let alpn = connection.alpn().unwrap();
        
        if alpn != P2P_ALPN.to_vec() {
            warn!("未知的ALPN协议: {:?}", alpn);
            return Ok(());
        }

        let remote_id = connection.remote_node_id()?;
        info!("接受来自 {} 的连接", remote_id);

        let (send, mut recv) = connection.accept_bi().await?;
        info!("建立双向流成功");
        
        // 发送一个初始消息
        let mut send = Arc::new(tokio::sync::Mutex::new(send));
        {
            let mut send_guard = send.lock().await;
            if let Err(e) = send_guard.write_all(b"CONNECTED\n").await {
                error!("发送初始消息失败: {:?}", e);
            }
            send_guard.flush().await?;
        }

        {
            let mut conns = connections.write().await;
            conns.insert(remote_id, PeerConnection {
                connection,
                send_stream: send,
            });
            info!("保存连接信息成功，当前连接数: {}", conns.len());
        }

        // 处理接收到的消息
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                match recv.read(&mut buf).await {
                    Ok(Some(0)) => {
                        info!("连接关闭");
                        break;
                    }
                    Ok(Some(n)) => {
                        info!("收到 {} 字节的消息", n);
                        if let Err(e) = message_tx
                            .send(Message {
                                from: remote_id,
                                payload: buf[..n].to_vec(),
                            })
                        {
                            error!("发送消息到channel失败: {:?}", e);
                            break;
                        }
                    }
                    Ok(None) => {
                        info!("流结束");
                        break;
                    }
                    Err(e) => {
                        error!("读取消息失败: {:?}", e);
                        break;
                    }
                }
            }
            info!("消息接收循环结束");
        });

        Ok(())
    }

    pub async fn connect(&self, addr: NodeAddr) -> Result<()> {
        info!("开始连接到节点: {:?}", addr);
        
        let connection = self
            .endpoint
            .connect(addr, P2P_ALPN)
            .await?;
            
        let remote_id = connection.remote_node_id()?;
        info!("连接成功远程节点ID: {}", remote_id);
        
        let (send, mut recv) = connection.open_bi().await?;
        info!("建立双向流成功");

        // 发送一个初始消息
        let send = Arc::new(tokio::sync::Mutex::new(send));
        {
            let mut send_guard = send.lock().await;
            if let Err(e) = send_guard.write_all(b"CONNECTED\n").await {
                error!("发送初始消息失败: {:?}", e);
            }
            send_guard.flush().await?;
        }

        {
            let mut conns = self.connections.write().await;
            conns.insert(remote_id, PeerConnection {
                connection,
                send_stream: send,
            });
            info!("保存连接信息成功，当前连接数: {}", conns.len());
        }

        let message_tx = self.message_tx.clone();
        
        // 处理接收到的消息
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                match recv.read(&mut buf).await {
                    Ok(Some(0)) => {
                        info!("连接关闭");
                        break;
                    }
                    Ok(Some(n)) => {
                        info!("收到 {} 字节的消息", n);
                        if let Err(e) = message_tx
                            .send(Message {
                                from: remote_id,
                                payload: buf[..n].to_vec(),
                            })
                        {
                            error!("发送消息到channel失败: {:?}", e);
                            break;
                        }
                    }
                    Ok(None) => {
                        info!("流结束");
                        break;
                    }
                    Err(e) => {
                        error!("读取消息失败: {:?}", e);
                        break;
                    }
                }
            }
            info!("消息接收循环结束");
        });

        Ok(())
    }

    pub async fn send_message(&self, to: PublicKey, payload: Vec<u8>) -> Result<()> {
        let connections = self.connections.read().await;
        if let Some(peer) = connections.get(&to) {
            info!("发送消息到 {}", to);
            let mut send_guard = peer.send_stream.lock().await;
            send_guard.write_all(&payload).await?;
            send_guard.write_all(b"\n").await?;
            send_guard.flush().await?;
            info!("消息发送成功");
            Ok(())
        } else {
            error!("未找到与节点 {} 的连接", to);
            Err(anyhow::anyhow!("未找到与目标节点的连接"))
        }
    }

    pub fn receive_messages(&self) -> broadcast::Receiver<Message> {
        info!("获取消息接收通道");
        self.message_tx.subscribe()
    }

    pub async fn disconnect(&self, peer: PublicKey) -> Result<()> {
        let mut connections = self.connections.write().await;
        if connections.remove(&peer).is_some() {
            info!("断开与节点 {} 的连接", peer);
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        let mut connections = self.connections.write().await;
        info!("关闭所有连接，当前连接数: {}", connections.len());
        connections.clear();
        Ok(())
    }

    async fn wait_for_relay(&self) -> Result<()> {
        info!("等待中继服务器初始化...");
        let relay = self.endpoint.home_relay().initialized().await?;
        info!("中继服务器就绪: {}", relay);
        Ok(())
    }

    pub fn get_node_id(&self) -> PublicKey {
        self.public_key
    }

    pub async fn get_node_addr(&self) -> Result<NodeAddr> {
        self.endpoint.node_addr().await
    }

    pub async fn get_connections(&self) -> Vec<PublicKey> {
        let connections = self.connections.read().await;
        let peers = connections.keys().cloned().collect();
        info!("当前连接的节点: {:?}", peers);
        peers
    }

    pub async fn check_connection(&self, peer: PublicKey) -> Result<bool> {
        let connections = self.connections.read().await;
        if let Some(peer_conn) = connections.get(&peer) {
            Ok(peer_conn.is_alive().await)
        } else {
            Ok(false)
        }
    }

    pub async fn cleanup_dead_connections(&self) -> Result<()> {
        let mut connections = self.connections.write().await;
        let mut dead_peers = Vec::new();
        
        for (peer, conn) in connections.iter() {
            if !conn.is_alive().await {
                dead_peers.push(*peer);
            }
        }
        
        for peer in dead_peers {
            connections.remove(&peer);
            info!("移除断开的连接: {}", peer);
        }
        
        Ok(())
    }
} 