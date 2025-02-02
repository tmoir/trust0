use std::collections::HashMap;
use std::ops::DerefMut;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;

use anyhow::Result;

use super::proxy::proxy_base::GatewayServiceProxy;
use super::proxy::tcp_proxy::TcpGatewayProxy;
use crate::config::AppConfig;
use crate::service::proxy::proxy_base::GatewayServiceProxyVisitor;
use crate::service::proxy::tcp_proxy::TcpGatewayProxyServerVisitor;
use crate::service::proxy::udp_proxy::{UdpGatewayProxy, UdpGatewayProxyServerVisitor};
use trust0_common::error::AppError;
use trust0_common::logging::info;
use trust0_common::model::service::{Service, Transport};
use trust0_common::proxy::event::ProxyEvent;
use trust0_common::proxy::executor::ProxyExecutorEvent;
use trust0_common::target;

const DEFAULT_SERVICE_PORT_START: u16 = 8200;
const DEFAULT_SERVICE_PORT_END: u16 = 8250;

/// Handles management of service proxy connections
pub trait ServiceMgr: Send {
    /// Return service ID for given proxy key, else return None
    fn get_service_id_by_proxy_key(&self, proxy_key: &str) -> Option<u64>;

    /// Active service proxy visitors accessor
    fn get_service_proxies(&self) -> Vec<Arc<Mutex<dyn GatewayServiceProxyVisitor>>>;
    /// Service proxy visitor (by service ID) accessor
    fn get_service_proxy(
        &self,
        service_id: u64,
    ) -> Option<&Arc<Mutex<dyn GatewayServiceProxyVisitor>>>;
    /// Clone proxy tasks sender
    fn clone_proxy_tasks_sender(&self) -> Sender<ProxyExecutorEvent>;
    /// Startup new proxy service to allow clients to connect/communicate to given service
    /// Returns service proxy address/port
    fn startup(
        &mut self,
        service_mgr: Arc<Mutex<dyn ServiceMgr>>,
        service: &Service,
    ) -> Result<(Option<String>, u16), AppError>;
    /// Returns whether there is an active service proxy for given user and service
    fn has_proxy_for_user_and_service(&mut self, user_id: u64, service_id: u64) -> bool;
    /// Shutdown service proxy connections. Consider all proxies or by service and/or user (if supplied).
    fn shutdown_connections(
        &mut self,
        user_id: Option<u64>,
        service_id: Option<u64>,
    ) -> Result<(), AppError>;

    /// Perform cleanup for a closed proxy
    fn on_closed_proxy(&mut self, proxy_key: &str);
}

/// Manage (Gateway <-> Service) service connections. Only one of these should be constructed.
pub struct GatewayServiceMgr {
    app_config: Arc<AppConfig>,
    service_proxies: HashMap<u64, Arc<Mutex<dyn GatewayServiceProxy>>>,
    service_proxy_visitors: HashMap<u64, Arc<Mutex<dyn GatewayServiceProxyVisitor>>>,
    service_proxy_threads: HashMap<u64, JoinHandle<Result<(), AppError>>>,
    services_by_proxy_key: Arc<Mutex<HashMap<String, u64>>>,
    service_ports: HashMap<u64, u16>,
    shared_service_port: Option<u16>,
    next_service_port: u16,
    last_service_port: u16,
    proxy_events_sender: Sender<ProxyEvent>,
    proxy_tasks_sender: Sender<ProxyExecutorEvent>,
}

impl GatewayServiceMgr {
    /// ServiceMgr constructor
    pub fn new(
        app_config: Arc<AppConfig>,
        proxy_tasks_sender: Sender<ProxyExecutorEvent>,
        proxy_events_sender: Sender<ProxyEvent>,
    ) -> Self {
        let mut next_service_port = DEFAULT_SERVICE_PORT_START;
        let mut last_service_port = DEFAULT_SERVICE_PORT_END;
        let mut shared_service_port = None;

        match &app_config.gateway_service_ports {
            Some((port_start, port_end)) => {
                next_service_port = *port_start;
                last_service_port = *port_end;
            }
            None => {
                shared_service_port = Some(app_config.server_port);
            }
        };

        Self {
            app_config,
            service_proxies: HashMap::new(),
            service_proxy_visitors: HashMap::new(),
            service_proxy_threads: HashMap::new(),
            service_ports: HashMap::new(),
            services_by_proxy_key: Arc::new(Mutex::new(HashMap::new())),
            shared_service_port,
            next_service_port,
            last_service_port,
            proxy_events_sender,
            proxy_tasks_sender,
        }
    }

    /// Listen and process any proxy events (blocking)
    pub fn poll_proxy_events(
        service_mgr: Arc<Mutex<dyn ServiceMgr>>,
        proxy_events_receiver: Receiver<ProxyEvent>,
    ) -> Result<(), AppError> {
        loop {
            // Get next request task
            let proxy_event = proxy_events_receiver.recv().map_err(|err| {
                AppError::GenWithMsgAndErr("Error receiving proxy event".to_string(), Box::new(err))
            })?;

            // Process event
            match proxy_event {
                ProxyEvent::Closed(proxy_key) => {
                    service_mgr.lock().unwrap().on_closed_proxy(&proxy_key);
                }

                ProxyEvent::Message(_, _, _) => {
                    unimplemented!();
                }
            }
        }
    }
}

impl ServiceMgr for GatewayServiceMgr {
    fn get_service_id_by_proxy_key(&self, proxy_key: &str) -> Option<u64> {
        self.services_by_proxy_key
            .lock()
            .unwrap()
            .get(proxy_key)
            .cloned()
    }

    fn get_service_proxies(&self) -> Vec<Arc<Mutex<dyn GatewayServiceProxyVisitor>>> {
        self.service_proxy_visitors.values().cloned().collect()
    }
    fn get_service_proxy(
        &self,
        service_id: u64,
    ) -> Option<&Arc<Mutex<dyn GatewayServiceProxyVisitor>>> {
        self.service_proxy_visitors.get(&service_id)
    }
    fn clone_proxy_tasks_sender(&self) -> Sender<ProxyExecutorEvent> {
        self.proxy_tasks_sender.clone()
    }
    fn startup(
        &mut self,
        service_mgr: Arc<Mutex<dyn ServiceMgr>>,
        service: &Service,
    ) -> Result<(Option<String>, u16), AppError> {
        // Check if already started
        // - - - - - - - - - - - -
        if let Some(service_port) = self.service_ports.get(&service.service_id) {
            return Ok((self.app_config.gateway_service_host.clone(), *service_port));
        }

        // Startup new proxy for service
        // - - - - - - - - - - - - - - -
        let service_port = match self.shared_service_port {
            Some(port) => port,
            None => {
                if self.next_service_port > self.last_service_port {
                    return Err(AppError::General(
                        "Service ports exhausted, please extend range".to_string(),
                    ));
                }
                self.next_service_port += 1;
                self.next_service_port - 1
            }
        };

        let service_proxy: Arc<Mutex<dyn GatewayServiceProxy>>;
        let service_proxy_visitor: Arc<Mutex<dyn GatewayServiceProxyVisitor>>;
        let mut service_proxy_thread: Option<JoinHandle<Result<(), AppError>>> = None;

        match service.transport {
            // Starts up TCP service proxy
            Transport::TCP => {
                // Setup service proxy objects
                let tcp_proxy_visitor = Arc::new(Mutex::new(TcpGatewayProxyServerVisitor::new(
                    self.app_config.clone(),
                    service_mgr.clone(),
                    service.clone(),
                    self.app_config.gateway_service_host.clone(),
                    service_port,
                    self.proxy_tasks_sender.clone(),
                    self.proxy_events_sender.clone(),
                    self.services_by_proxy_key.clone(),
                )?));

                service_proxy = Arc::new(Mutex::new(TcpGatewayProxy::new(
                    self.app_config.clone(),
                    tcp_proxy_visitor.clone(),
                    service_port,
                )));

                service_proxy_visitor = tcp_proxy_visitor;

                // Startup service proxy listener (only if not using shared listener port)
                if self.shared_service_port.is_none() {
                    let service_proxy_closure = service_proxy.clone();
                    service_proxy_thread = Some(thread::spawn(move || {
                        service_proxy_closure.lock().unwrap().startup()
                    }));
                }
            }

            // Starts up UDP service proxy
            Transport::UDP => {
                // Setup service proxy objects
                let udp_proxy_visitor = Arc::new(Mutex::new(UdpGatewayProxyServerVisitor::new(
                    self.app_config.clone(),
                    service_mgr.clone(),
                    service.clone(),
                    self.app_config.gateway_service_host.clone(),
                    service_port,
                    self.proxy_tasks_sender.clone(),
                    self.proxy_events_sender.clone(),
                    self.services_by_proxy_key.clone(),
                )?));

                service_proxy = Arc::new(Mutex::new(UdpGatewayProxy::new(
                    self.app_config.clone(),
                    udp_proxy_visitor.clone(),
                    service_port,
                )));

                service_proxy_visitor = udp_proxy_visitor;

                // Startup service proxy listener (only if not using shared listener port)
                if self.shared_service_port.is_none() {
                    let service_proxy_closure = service_proxy.clone();
                    service_proxy_thread = Some(thread::spawn(move || {
                        service_proxy_closure.lock().unwrap().startup()
                    }));
                }
            }
        }

        self.service_ports.insert(service.service_id, service_port);
        self.service_proxies
            .insert(service.service_id, service_proxy);
        self.service_proxy_visitors
            .insert(service.service_id, service_proxy_visitor);

        if let Some(thread) = service_proxy_thread {
            self.service_proxy_threads
                .insert(service.service_id, thread);
        }

        Ok((self.app_config.gateway_service_host.clone(), service_port))
    }
    fn has_proxy_for_user_and_service(&mut self, user_id: u64, service_id: u64) -> bool {
        match self.service_proxy_visitors.get(&service_id) {
            Some(proxy_visitor) => {
                let proxy_visitor = proxy_visitor.lock().unwrap();
                !proxy_visitor.get_proxy_addrs_for_user(user_id).is_empty()
            }

            None => false,
        }
    }
    fn shutdown_connections(
        &mut self,
        user_id: Option<u64>,
        service_id: Option<u64>,
    ) -> Result<(), AppError> {
        let mut errors: Vec<String> = vec![];

        self.service_proxy_visitors.iter().for_each(|(proxy_service_id, proxy_visitor)| {
            if service_id.is_none() || (*proxy_service_id == service_id.unwrap()) {
                let mut proxy_visitor = proxy_visitor.lock().unwrap();

                if let Err(err) = proxy_visitor.deref_mut().shutdown_connections(self.clone_proxy_tasks_sender(), user_id) {
                    errors.push(format!("Failed shutting down service proxy connection: svc_id={}, user_id={:?}, err={:?}", proxy_service_id, user_id, err));
                } else {
                    info(&target!(), &format!("Service proxy connection shutdown: svc_id={}, user_id={:?}", proxy_service_id, user_id));
                }
            }
        });

        if !errors.is_empty() {
            return Err(AppError::General(format!(
                "Error shutting down services: user_id={:?}, err(s)={}",
                user_id,
                errors.join(",")
            )));
        }

        Ok(())
    }

    fn on_closed_proxy(&mut self, proxy_key: &str) {
        let service_id = self
            .get_service_id_by_proxy_key(proxy_key)
            .unwrap_or(u64::MAX);
        if let Some(proxy_visitor) = self.get_service_proxy(service_id) {
            proxy_visitor
                .lock()
                .unwrap()
                .remove_proxy_for_key(proxy_key);
        }
    }
}

/// Unit tests
#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::config;
    use crate::repository::access_repo::tests::MockAccessRepo;
    use crate::repository::service_repo::tests::MockServiceRepo;
    use crate::repository::user_repo::tests::MockUserRepo;
    use crate::service::proxy::proxy_base::tests::MockGwSvcProxyVisitor;
    use mockall::{mock, predicate};
    use std::sync::mpsc;

    // mocks
    // =====

    mock! {
        pub SvcMgr {}
        impl ServiceMgr for SvcMgr {
            fn get_service_id_by_proxy_key(&self, proxy_key: &str) -> Option<u64>;
            fn get_service_proxies(&self) -> Vec<Arc<Mutex<dyn GatewayServiceProxyVisitor>>>;
            fn get_service_proxy(&self, service_id: u64) -> Option<&'static Arc<Mutex<dyn GatewayServiceProxyVisitor>>>;
            fn clone_proxy_tasks_sender(&self) -> Sender<ProxyExecutorEvent>;
            fn startup(&mut self, service_mgr: Arc<Mutex<dyn ServiceMgr>>, service: &Service) -> Result<(Option<String>, u16), AppError>;
            fn has_proxy_for_user_and_service(&mut self, user_id: u64, service_id: u64) -> bool;
            fn shutdown_connections(&mut self, user_id: Option<u64>, service_id: Option<u64>) -> Result<(), AppError>;
            fn on_closed_proxy(&mut self, proxy_key: &str);
        }
    }

    // GatewayServiceMgr tests
    // =======================
    const GATEWAY_HOST: &str = "gwhost1";
    const GATEWAY_SHARED_PORT: u16 = 4000;
    const GATEWAY_DISTINCT_PORT_START: u16 = 4100;
    const GATEWAY_DISTINCT_PORT_END: u16 = 4102;

    fn create_gw_service_mgr(use_shared_port: bool) -> GatewayServiceMgr {
        let mut app_config = config::tests::create_app_config_with_repos(
            Arc::new(Mutex::new(MockUserRepo::new())),
            Arc::new(Mutex::new(MockServiceRepo::new())),
            Arc::new(Mutex::new(MockAccessRepo::new())),
        )
        .unwrap();
        app_config.gateway_service_host = Some(GATEWAY_HOST.to_string());
        if !use_shared_port {
            app_config.gateway_service_ports =
                Some((GATEWAY_DISTINCT_PORT_START, GATEWAY_DISTINCT_PORT_END));
        }

        let mut service_mgr =
            GatewayServiceMgr::new(Arc::new(app_config), mpsc::channel().0, mpsc::channel().0);
        if use_shared_port {
            service_mgr.shared_service_port = Some(GATEWAY_SHARED_PORT);
        }
        service_mgr
    }
    #[test]
    fn gwsvcmgr_startup_when_already_started() {
        let service = Service {
            service_id: 200,
            name: "Service200".to_string(),
            transport: Transport::TCP,
            host: "localhost".to_string(),
            port: 8200,
        };
        let mut service_mgr = create_gw_service_mgr(true);
        service_mgr
            .service_ports
            .insert(service.service_id, GATEWAY_SHARED_PORT);
        let orig_svc_ports_len = service_mgr.service_ports.len();
        let orig_svc_proxies_len = service_mgr.service_proxies.len();
        let orig_svc_proxy_visitors_len = service_mgr.service_proxy_visitors.len();
        let service_mgr = Arc::new(Mutex::new(service_mgr));

        match service_mgr
            .lock()
            .unwrap()
            .startup(service_mgr.clone(), &service)
        {
            Ok((host, port)) => {
                assert!(host.is_some());
                assert_eq!(host.unwrap(), GATEWAY_HOST.to_string());
                assert_eq!(port, GATEWAY_SHARED_PORT);
            }
            Err(err) => {
                panic!("Unexpected startup result: err={:?}", &err);
            }
        }

        assert_eq!(
            service_mgr.lock().unwrap().service_ports.len(),
            orig_svc_ports_len
        );
        assert_eq!(
            service_mgr.lock().unwrap().service_proxies.len(),
            orig_svc_proxies_len
        );
        assert_eq!(
            service_mgr.lock().unwrap().service_proxy_visitors.len(),
            orig_svc_proxy_visitors_len
        );
    }

    #[test]
    fn gwsvcmgr_startup_when_exhausted_ports() {
        let service = Service {
            service_id: 200,
            name: "Service200".to_string(),
            transport: Transport::TCP,
            host: "localhost".to_string(),
            port: 8200,
        };
        let mut service_mgr = create_gw_service_mgr(false);
        service_mgr.next_service_port = GATEWAY_DISTINCT_PORT_END + 1;
        let orig_svc_ports_len = service_mgr.service_ports.len();
        let orig_svc_proxies_len = service_mgr.service_proxies.len();
        let orig_svc_proxy_visitors_len = service_mgr.service_proxy_visitors.len();
        let service_mgr = Arc::new(Mutex::new(service_mgr));

        match service_mgr
            .lock()
            .unwrap()
            .startup(service_mgr.clone(), &service)
        {
            Ok((host, port)) => {
                panic!("Unexpected startup result: host={:?}, port={}", host, port);
            }
            Err(err) => {
                if !err.to_string().contains("exhausted") {
                    panic!("Unexpected startup result: err={:?}", &err);
                }
            }
        }

        assert_eq!(
            service_mgr.lock().unwrap().service_ports.len(),
            orig_svc_ports_len
        );
        assert_eq!(
            service_mgr.lock().unwrap().service_proxies.len(),
            orig_svc_proxies_len
        );
        assert_eq!(
            service_mgr.lock().unwrap().service_proxy_visitors.len(),
            orig_svc_proxy_visitors_len
        );
    }

    #[test]
    fn gwsvcmgr_startup_when_tcp_service() {
        let service = Service {
            service_id: 200,
            name: "Service200".to_string(),
            transport: Transport::TCP,
            host: "localhost".to_string(),
            port: 8200,
        };
        let service_mgr = create_gw_service_mgr(true);
        let orig_svc_ports_len = service_mgr.service_ports.len();
        let orig_svc_proxies_len = service_mgr.service_proxies.len();
        let orig_svc_proxy_visitors_len = service_mgr.service_proxy_visitors.len();
        let service_mgr = Arc::new(Mutex::new(service_mgr));

        match service_mgr
            .clone()
            .lock()
            .unwrap()
            .startup(service_mgr.clone(), &service)
        {
            Ok((host, port)) => {
                assert!(host.is_some());
                assert_eq!(host.unwrap(), GATEWAY_HOST.to_string());
                assert_eq!(port, GATEWAY_SHARED_PORT);
            }
            Err(err) => {
                panic!("Unexpected startup result: err={:?}", &err);
            }
        }

        assert_eq!(
            service_mgr.lock().unwrap().service_ports.len(),
            orig_svc_ports_len + 1
        );
        assert_eq!(
            service_mgr.lock().unwrap().service_proxies.len(),
            orig_svc_proxies_len + 1
        );
        assert_eq!(
            service_mgr.lock().unwrap().service_proxy_visitors.len(),
            orig_svc_proxy_visitors_len + 1
        );
    }

    #[test]
    fn gwsvcmgr_startup_when_udp_service() {
        let service = Service {
            service_id: 200,
            name: "Service200".to_string(),
            transport: Transport::UDP,
            host: "localhost".to_string(),
            port: 8200,
        };
        let service_mgr = create_gw_service_mgr(true);
        let orig_svc_ports_len = service_mgr.service_ports.len();
        let orig_svc_proxies_len = service_mgr.service_proxies.len();
        let orig_svc_proxy_visitors_len = service_mgr.service_proxy_visitors.len();
        let service_mgr = Arc::new(Mutex::new(service_mgr));

        match service_mgr
            .clone()
            .lock()
            .unwrap()
            .startup(service_mgr.clone(), &service)
        {
            Ok((host, port)) => {
                assert!(host.is_some());
                assert_eq!(host.unwrap(), GATEWAY_HOST.to_string());
                assert_eq!(port, GATEWAY_SHARED_PORT);
            }
            Err(err) => {
                panic!("Unexpected startup result: err={:?}", &err);
            }
        }

        assert_eq!(
            service_mgr.lock().unwrap().service_ports.len(),
            orig_svc_ports_len + 1
        );
        assert_eq!(
            service_mgr.lock().unwrap().service_proxies.len(),
            orig_svc_proxies_len + 1
        );
        assert_eq!(
            service_mgr.lock().unwrap().service_proxy_visitors.len(),
            orig_svc_proxy_visitors_len + 1
        );
    }

    #[test]
    fn gwsvcmgr_has_proxy_for_user_and_service_when_valid_user_and_svc() {
        let mut proxy_visitor = MockGwSvcProxyVisitor::new();
        proxy_visitor
            .expect_get_proxy_addrs_for_user()
            .with(predicate::eq(100))
            .times(1)
            .return_once(move |_| vec![("addr1".to_string(), "addr2".to_string())]);
        let mut service_mgr = create_gw_service_mgr(true);
        service_mgr
            .service_proxy_visitors
            .insert(200, Arc::new(Mutex::new(proxy_visitor)));

        assert!(service_mgr.has_proxy_for_user_and_service(100, 200));
    }

    #[test]
    fn gwsvcmgr_has_proxy_for_user_and_service_when_invalid_user() {
        let mut proxy_visitor = MockGwSvcProxyVisitor::new();
        proxy_visitor
            .expect_get_proxy_addrs_for_user()
            .with(predicate::eq(101))
            .times(1)
            .return_once(move |_| vec![]);
        let mut service_mgr = create_gw_service_mgr(true);
        service_mgr
            .service_proxy_visitors
            .insert(200, Arc::new(Mutex::new(proxy_visitor)));

        assert!(!service_mgr.has_proxy_for_user_and_service(101, 200));
    }

    #[test]
    fn gwsvcmgr_has_proxy_for_user_and_service_when_invalid_service() {
        let mut proxy_visitor = MockGwSvcProxyVisitor::new();
        proxy_visitor
            .expect_get_proxy_addrs_for_user()
            .with(predicate::always())
            .never();
        let mut service_mgr = create_gw_service_mgr(true);
        service_mgr
            .service_proxy_visitors
            .insert(200, Arc::new(Mutex::new(proxy_visitor)));

        assert!(!service_mgr.has_proxy_for_user_and_service(100, 201));
    }

    #[test]
    fn gwsvcmgr_shutdown_connections_when_no_service_or_user_given() {
        let mut proxy200_visitor = MockGwSvcProxyVisitor::new();
        proxy200_visitor
            .expect_shutdown_connections()
            .with(predicate::always(), predicate::eq(None))
            .times(1)
            .return_once(move |_, _| Ok(()));
        let mut proxy201_visitor = MockGwSvcProxyVisitor::new();
        proxy201_visitor
            .expect_shutdown_connections()
            .with(predicate::always(), predicate::eq(None))
            .times(1)
            .return_once(move |_, _| Ok(()));
        let mut service_mgr = create_gw_service_mgr(true);
        service_mgr
            .service_proxy_visitors
            .insert(200, Arc::new(Mutex::new(proxy200_visitor)));
        service_mgr
            .service_proxy_visitors
            .insert(201, Arc::new(Mutex::new(proxy201_visitor)));

        let result = service_mgr.shutdown_connections(None, None);

        if let Err(err) = &result {
            panic!("Unexpected shutdown result: err={:?}", &err);
        }
    }

    #[test]
    fn gwsvcmgr_shutdown_connections_when_no_service_given() {
        let mut proxy200_visitor = MockGwSvcProxyVisitor::new();
        proxy200_visitor
            .expect_shutdown_connections()
            .with(predicate::always(), predicate::eq(Some(100)))
            .times(1)
            .return_once(move |_, _| Ok(()));
        let mut proxy201_visitor = MockGwSvcProxyVisitor::new();
        proxy201_visitor
            .expect_shutdown_connections()
            .with(predicate::always(), predicate::eq(Some(100)))
            .times(1)
            .return_once(move |_, _| Ok(()));
        let mut service_mgr = create_gw_service_mgr(true);
        service_mgr
            .service_proxy_visitors
            .insert(200, Arc::new(Mutex::new(proxy200_visitor)));
        service_mgr
            .service_proxy_visitors
            .insert(201, Arc::new(Mutex::new(proxy201_visitor)));

        let result = service_mgr.shutdown_connections(Some(100), None);

        if let Err(err) = &result {
            panic!("Unexpected shutdown result: err={:?}", &err);
        }
    }

    #[test]
    fn gwsvcmgr_shutdown_connections_when_no_user_given() {
        let mut proxy200_visitor = MockGwSvcProxyVisitor::new();
        proxy200_visitor
            .expect_shutdown_connections()
            .with(predicate::always(), predicate::eq(None))
            .times(1)
            .return_once(move |_, _| Ok(()));
        let mut proxy201_visitor = MockGwSvcProxyVisitor::new();
        proxy201_visitor.expect_shutdown_connections().never();
        let mut service_mgr = create_gw_service_mgr(true);
        service_mgr
            .service_proxy_visitors
            .insert(200, Arc::new(Mutex::new(proxy200_visitor)));
        service_mgr
            .service_proxy_visitors
            .insert(201, Arc::new(Mutex::new(proxy201_visitor)));

        let result = service_mgr.shutdown_connections(None, Some(200));

        if let Err(err) = &result {
            panic!("Unexpected shutdown result: err={:?}", &err);
        }
    }

    #[test]
    fn gwsvcmgr_on_closed_proxy_when_valid_proxy_key() {
        let mut proxy_visitor = MockGwSvcProxyVisitor::new();
        proxy_visitor
            .expect_remove_proxy_for_key()
            .with(predicate::eq("key200"))
            .times(1)
            .return_once(move |_| true);
        let mut service_mgr = create_gw_service_mgr(true);
        service_mgr
            .services_by_proxy_key
            .lock()
            .unwrap()
            .insert("key200".to_string(), 200);
        service_mgr
            .service_proxy_visitors
            .insert(200, Arc::new(Mutex::new(proxy_visitor)));

        service_mgr.on_closed_proxy("key200");
    }
}
