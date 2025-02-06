use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use iroh::{Endpoint, NodeAddr, PublicKey, SecretKey};
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn, error};

const P2P_ALPN: &[u8] = b"P2P_PROTO_V1";

#[derive(Debug)]
struct PeerConnection {
    #[allow(dead_code)]
    connection: iroh::endpoint::Connection,
    send_stream: Arc<tokio::sync::Mutex<iroh::endpoint::SendStream>>,
}

impl PeerConnection {
    /// 检查连接是否仍然存活
    /// 
    /// 通过尝试发送空数据包来检测连接状态
    /// 
    /// 返回值:
    /// - true: 连接正常
    /// - false: 连接已断开
    async fn is_alive(&self) -> bool {
        let mut send_guard = match self.send_stream.try_lock() {
            Ok(guard) => guard,
            Err(_) => return true,
        };

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
}

impl P2PNode {
    /// 创建新的 P2P 节点实例
    /// 
    /// 参数:
    /// - secret_key: 可选的节点密钥，如果不提供则自动生成
    /// 
    /// 返回:
    /// - Result<Self>: 成功则返回节点实例，失败则返回错误
    pub async fn new(secret_key: Option<SecretKey>) -> Result<Self> {
        let secret_key = secret_key.unwrap_or_else(|| SecretKey::generate(rand::rngs::OsRng));
        let public_key = secret_key.public();
        
        info!("正在创建新节点，公钥为: {}", public_key);
        
        let endpoint = Arc::new(
            Endpoint::builder()
                .secret_key(secret_key)
                .alpns(vec![P2P_ALPN.to_vec()])
                .bind_addr_v4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
                .bind()
                .await?
        );

        Ok(Self {
            endpoint,
            public_key,
            connections: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// 开始监听传入的连接请求
    /// 
    /// 参数:
    /// - on_receive: 消息处理回调函数，当收到消息时被调用
    /// 
    /// 该函数会启动一个后台任务持续监听新的连接
    pub async fn start_listening<F, Fut>(&self, on_receive: F) -> Result<()> 
    where
        F: Fn(PublicKey, Vec<u8>) -> Fut + Send + Clone + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        info!("等待中继连接...");
        self.wait_for_relay().await?;
        
        let addr = self.endpoint.node_addr().await?;
        info!("节点 ID: {}", addr.node_id);
        info!("监听地址: {:#?}", addr);
        
        let connections = self.connections.clone();
        let endpoint = self.endpoint.clone();

        tokio::spawn(async move {
            info!("开始监听传入连接");
            while let Some(incoming) = endpoint.accept().await {
                info!("收到新的连接请求");
                let connections = connections.clone();
                let on_receive = on_receive.clone();
                
                tokio::spawn(async move {
                    if let Err(e) = Self::handle_incoming(incoming, connections, on_receive).await {
                        error!("处理连接时出错: {:?}", e);
                    }
                });
            }
        });

        Ok(())
    }

    /// 处理新建立的连接
    /// 
    /// 参数:
    /// - incoming: 传入的连接请求
    /// - connections: 连接管理器
    /// - on_receive: 消息处理回调函数
    /// 
    /// 该函数负责:
    /// 1. 验证连接协议
    /// 2. 建立双向数据流
    /// 3. 保存连接信息
    /// 4. 启动消息接收循环
    async fn handle_incoming<F, Fut>(
        incoming: iroh::endpoint::Incoming,
        connections: Arc<RwLock<HashMap<PublicKey, PeerConnection>>>,
        on_receive: F,
    ) -> Result<()>
    where
        F: Fn(PublicKey, Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let connection = incoming.await?;
        let alpn = connection.alpn().unwrap();
        
        if alpn != P2P_ALPN.to_vec() {
            warn!("未知的 ALPN 协议: {:?}", alpn);
            return Ok(());
        }

        let remote_id = connection.remote_node_id()?;
        info!("接受来自 {} 的连接", remote_id);

        let (send, mut recv) = connection.accept_bi().await?;
        info!("已建立双向流");
        
        let send = Arc::new(tokio::sync::Mutex::new(send));
        {
            let mut send_guard = send.lock().await;
            if let Err(e) = send_guard.write_all(b"CONNECTED\n").await {
                error!("Failed to send initial message: {:?}", e);
            }
            send_guard.flush().await?;
        }

        {
            let mut conns = connections.write().await;
            conns.insert(remote_id, PeerConnection {
                connection,
                send_stream: send,
            });
            info!("Connection info saved, current connections: {}", conns.len());
        }

        let on_receive = Arc::new(on_receive);
        let remote_id = remote_id;
        
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                match recv.read(&mut buf).await {
                    Ok(Some(0)) => {
                        info!("连接已关闭");
                        break;
                    }
                    Ok(Some(n)) => {
                        info!("收到 {} 字节的消息", n);
                        on_receive(remote_id, buf[..n].to_vec()).await;
                    }
                    Ok(None) => {
                        info!("流已结束");
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

    /// 主动连接到远程节点
    /// 
    /// 参数:
    /// - addr: 目标节点地址
    /// - on_receive: 消息处理回调函数
    /// 
    /// 该函数会:
    /// 1. 建立连接
    /// 2. 初始化双向数据流
    /// 3. 保存连接信息
    /// 4. 启动消息接收循环
    pub async fn connect<F, Fut>(&self, addr: NodeAddr, on_receive: F) -> Result<()>
    where
        F: Fn(PublicKey, Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        info!("正在开始连接到节点: {:?}", addr);
        
        let connection = self
            .endpoint
            .connect(addr, P2P_ALPN)
            .await?;
            
        let remote_id = connection.remote_node_id()?;
        info!("成功连接到远程节点 ID: {}", remote_id);
        
        let (send, mut recv) = connection.open_bi().await?;
        info!("已建立双向流");

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
            info!("连接信息已保存，当前连接数: {}", conns.len());
        }

        let on_receive = Arc::new(on_receive);
        let remote_id = remote_id;
        
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                match recv.read(&mut buf).await {
                    Ok(Some(0)) => {
                        info!("连接已关闭");
                        break;
                    }
                    Ok(Some(n)) => {
                        info!("收到 {} 字节的消息", n);
                        on_receive(remote_id, buf[..n].to_vec()).await;
                    }
                    Ok(None) => {
                        info!("流已结束");
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

    /// 向指定节点发送消息
    /// 
    /// 参数:
    /// - to: 目标节点的公钥
    /// - payload: 要发送的消息内容
    /// 
    /// 返回:
    /// - Ok(()): 发送成功
    /// - Err: 发送失败（可能是连接不存在或已断开）
    pub async fn send_message(&self, to: PublicKey, payload: Vec<u8>) -> Result<()> {
        let connections = self.connections.read().await;
        if let Some(peer) = connections.get(&to) {
            info!("正在发送消息给 {}", to);
            let mut send_guard = peer.send_stream.lock().await;
            send_guard.write_all(&payload).await?;
            send_guard.write_all(b"\n").await?;
            send_guard.flush().await?;
            info!("消息发送成功");
            Ok(())
        } else {
            error!("未找到与节点 {} 的连接", to);
            Err(anyhow::anyhow!("未找到目标节点的连接"))
        }
    }

    /// 主动断开与指定节点的连接
    /// 
    /// 参数:
    /// - peer: 要断开连接的节点公钥
    pub async fn disconnect(&self, peer: PublicKey) -> Result<()> {
        let mut connections = self.connections.write().await;
        if connections.remove(&peer).is_some() {
            info!("已断开与节点 {} 的连接", peer);
        }
        Ok(())
    }

    /// 关闭所有连接并清理资源
    pub async fn shutdown(&self) -> Result<()> {
        let mut connections = self.connections.write().await;
        info!("正在关闭所有连接，当前连接数: {}", connections.len());
        connections.clear();
        Ok(())
    }

    /// 等待中继服务器初始化完成
    /// 
    /// 在节点开始工作之前确保中继服务可用
    async fn wait_for_relay(&self) -> Result<()> {
        info!("等待中继服务器初始化...");
        let relay = self.endpoint.home_relay().initialized().await?;
        info!("中继服务器就绪: {}", relay);
        Ok(())
    }

    /// 获取当前节点的公钥标识
    pub fn get_node_id(&self) -> PublicKey {
        self.public_key
    }

    /// 获取当前节点的网络地址
    pub async fn get_node_addr(&self) -> Result<NodeAddr> {
        self.endpoint.node_addr().await
    }

    /// 获取当前所有已连接节点的列表
    /// 
    /// 返回所有已连接节点的公钥列表
    pub async fn get_connections(&self) -> Vec<PublicKey> {
        let connections = self.connections.read().await;
        let peers = connections.keys().cloned().collect();
        info!("当前已连接的节点: {:?}", peers);
        peers
    }

    /// 检查与指定节点的连接状态
    /// 
    /// 参数:
    /// - peer: 要检查的节点公钥
    /// 
    /// 返回:
    /// - Ok(true): 连接正常
    /// - Ok(false): 连接不存在或已断开
    pub async fn check_connection(&self, peer: PublicKey) -> Result<bool> {
        let connections = self.connections.read().await;
        if let Some(peer_conn) = connections.get(&peer) {
            Ok(peer_conn.is_alive().await)
        } else {
            Ok(false)
        }
    }

    /// 清理所有已断开的连接
    /// 
    /// 定期调用此函数可以清理无效连接，释放资源
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
            info!("已移除断开连接的节点: {}", peer);
        }
        
        Ok(())
    }
} 