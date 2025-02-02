use std::collections::HashMap;
use std::fs;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::repository::service_repo::ServiceRepository;
use trust0_common::error::AppError;
use trust0_common::model::service::Service;

pub struct InMemServiceRepo {
    services: RwLock<HashMap<u64, Service>>,
}

impl InMemServiceRepo {
    /// Creates a new in-memory service store.
    pub fn new() -> InMemServiceRepo {
        InMemServiceRepo {
            services: RwLock::new(HashMap::new()),
        }
    }

    fn access_data_for_write(&self) -> Result<RwLockWriteGuard<HashMap<u64, Service>>, AppError> {
        self.services.write().map_err(|err| {
            AppError::General(format!("Failed to access write lock to DB: err={}", err))
        })
    }

    fn access_data_for_read(&self) -> Result<RwLockReadGuard<HashMap<u64, Service>>, AppError> {
        self.services.read().map_err(|err| {
            AppError::General(format!("Failed to access read lock to DB: err={}", err))
        })
    }
}

impl ServiceRepository for InMemServiceRepo {
    fn connect_to_datasource(&mut self, connect_spec: &str) -> Result<(), AppError> {
        let data = fs::read_to_string(connect_spec).map_err(|err| {
            AppError::GenWithMsgAndErr(
                format!("Failed to read file: path={}", connect_spec),
                Box::new(err),
            )
        })?;
        let services: Vec<Service> = serde_json::from_str(&data).map_err(|err| {
            AppError::GenWithMsgAndErr(
                format!("Failed to parse JSON: path={}", connect_spec),
                Box::new(err),
            )
        })?;

        for service in services.iter().as_ref() {
            self.put(service.clone())?;
        }

        Ok(())
    }

    fn put(&self, service: Service) -> Result<Option<Service>, AppError> {
        let mut data = self.access_data_for_write()?;
        Ok(data.insert(service.service_id, service.clone()))
    }

    fn get(&self, service_id: u64) -> Result<Option<Service>, AppError> {
        let data = self.access_data_for_read()?;
        Ok(data.get(&service_id).cloned())
    }

    fn get_all(&self) -> Result<Vec<Service>, AppError> {
        let data = self.access_data_for_read()?;
        Ok(data
            .iter()
            .map(|entry| entry.1)
            .cloned()
            .collect::<Vec<Service>>())
    }

    fn delete(&self, service_id: u64) -> Result<Option<Service>, AppError> {
        let mut data = self.access_data_for_write()?;
        Ok(data.remove(&service_id))
    }
}

/// Unit tests
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use trust0_common::model::service::Transport;

    const VALID_SERVICE_DB_FILE_PATHPARTS: [&str; 3] =
        [env!("CARGO_MANIFEST_DIR"), "testdata", "db-service.json"];
    const INVALID_SERVICE_DB_FILE_PATHPARTS: [&str; 3] = [
        env!("CARGO_MANIFEST_DIR"),
        "testdata",
        "db-service-INVALID.json",
    ];

    #[test]
    fn inmemsvcrepo_connect_to_datasource_when_invalid_filepath() {
        let invalid_service_db_path: PathBuf = INVALID_SERVICE_DB_FILE_PATHPARTS.iter().collect();
        let invalid_service_db_pathstr = invalid_service_db_path.to_str().unwrap();

        let mut service_repo = InMemServiceRepo::new();

        if let Ok(()) = service_repo.connect_to_datasource(invalid_service_db_pathstr) {
            panic!("Unexpected result: file={}", invalid_service_db_pathstr);
        }
    }

    #[test]
    fn inmemsvcrepo_connect_to_datasource_when_valid_filepath() {
        let valid_service_db_path: PathBuf = VALID_SERVICE_DB_FILE_PATHPARTS.iter().collect();
        let valid_service_db_pathstr = valid_service_db_path.to_str().unwrap();

        let mut service_repo = InMemServiceRepo::new();

        if let Err(err) = service_repo.connect_to_datasource(valid_service_db_pathstr) {
            panic!(
                "Unexpected result: file={}, err={:?}",
                valid_service_db_pathstr, &err
            );
        }

        let expected_service_db_map: HashMap<u64, Service> = HashMap::from([
            (
                200,
                Service {
                    service_id: 200,
                    name: "Service200".to_string(),
                    transport: Transport::TCP,
                    host: "localhost".to_string(),
                    port: 8200,
                },
            ),
            (
                201,
                Service {
                    service_id: 201,
                    name: "Service201".to_string(),
                    transport: Transport::TCP,
                    host: "localhost".to_string(),
                    port: 8201,
                },
            ),
            (
                202,
                Service {
                    service_id: 202,
                    name: "Service202".to_string(),
                    transport: Transport::TCP,
                    host: "localhost".to_string(),
                    port: 8202,
                },
            ),
            (
                203,
                Service {
                    service_id: 203,
                    name: "chat-tcp".to_string(),
                    transport: Transport::TCP,
                    host: "localhost".to_string(),
                    port: 8500,
                },
            ),
            (
                204,
                Service {
                    service_id: 204,
                    name: "echo-udp".to_string(),
                    transport: Transport::UDP,
                    host: "localhost".to_string(),
                    port: 8600,
                },
            ),
        ]);

        let actual_service_db_map: HashMap<u64, Service> = HashMap::from_iter(
            service_repo
                .services
                .into_inner()
                .unwrap()
                .iter()
                .map(|e| (e.0.clone(), e.1.clone()))
                .collect::<Vec<(u64, Service)>>(),
        );

        assert_eq!(actual_service_db_map.len(), expected_service_db_map.len());
        assert_eq!(
            actual_service_db_map
                .iter()
                .filter(|entry| !expected_service_db_map.contains_key(entry.0))
                .count(),
            0
        );
    }

    #[test]
    fn inmemsvcrepo_put() {
        let service_repo = InMemServiceRepo::new();
        let service_key = 1;
        let service = Service {
            service_id: 1,
            name: "svc1".to_string(),
            transport: Transport::TCP,
            host: "site1".to_string(),
            port: 100,
        };

        if let Err(err) = service_repo.put(service.clone()) {
            panic!("Unexpected result: err={:?}", &err)
        }

        let stored_map = service_repo.services.read().unwrap();
        let stored_entry = stored_map.get(&service_key);

        assert!(stored_entry.is_some());
        assert_eq!(*stored_entry.unwrap(), service);
    }

    #[test]
    fn inmemsvcrepo_get_when_invalid_service() {
        let service_repo = InMemServiceRepo::new();
        let service_key = 1;
        let service = Service {
            service_id: 1,
            name: "svc1".to_string(),
            transport: Transport::TCP,
            host: "site1".to_string(),
            port: 100,
        };

        service_repo
            .services
            .write()
            .unwrap()
            .insert(service_key, service);

        let result = service_repo.get(10);

        if let Err(err) = &result {
            panic!("Unexpected result: err={:?}", &err)
        }

        assert!(result.unwrap().is_none());
    }

    #[test]
    fn inmemsvcrepo_get_when_valid_service() {
        let service_repo = InMemServiceRepo::new();
        let service_keys = [1, 2, 3];
        let services = [
            Service {
                service_id: 1,
                name: "svc1".to_string(),
                transport: Transport::TCP,
                host: "site1".to_string(),
                port: 100,
            },
            Service {
                service_id: 2,
                name: "svc2".to_string(),
                transport: Transport::TCP,
                host: "site2".to_string(),
                port: 200,
            },
            Service {
                service_id: 3,
                name: "svc3".to_string(),
                transport: Transport::UDP,
                host: "site3".to_string(),
                port: 300,
            },
        ];

        service_repo
            .services
            .write()
            .unwrap()
            .insert(service_keys[0], services[0].clone());
        service_repo
            .services
            .write()
            .unwrap()
            .insert(service_keys[1], services[1].clone());
        service_repo
            .services
            .write()
            .unwrap()
            .insert(service_keys[2], services[2].clone());

        let result = service_repo.get(2);

        if let Err(err) = &result {
            panic!("Unexpected result: err={:?}", &err)
        }

        let actual_service = result.unwrap();

        assert!(actual_service.is_some());
        assert_eq!(actual_service.unwrap(), services[1]);
    }

    #[test]
    fn inmemsvcrepo_get_all() {
        let service_repo = InMemServiceRepo::new();
        let service_keys = [1, 2, 3];
        let services = [
            Service {
                service_id: 1,
                name: "svc1".to_string(),
                transport: Transport::TCP,
                host: "site1".to_string(),
                port: 100,
            },
            Service {
                service_id: 2,
                name: "svc2".to_string(),
                transport: Transport::TCP,
                host: "site2".to_string(),
                port: 200,
            },
            Service {
                service_id: 3,
                name: "svc3".to_string(),
                transport: Transport::UDP,
                host: "site3".to_string(),
                port: 300,
            },
        ];

        service_repo
            .services
            .write()
            .unwrap()
            .insert(service_keys[0], services[0].clone());
        service_repo
            .services
            .write()
            .unwrap()
            .insert(service_keys[1], services[1].clone());
        service_repo
            .services
            .write()
            .unwrap()
            .insert(service_keys[2], services[2].clone());

        let result = service_repo.get_all();

        if let Err(err) = &result {
            panic!("Unexpected result: err={:?}", &err)
        }

        let actual_services = result.unwrap();
        assert_eq!(actual_services.len(), 3);

        let expected_access_db_map: HashMap<u64, Service> = HashMap::from([
            (
                1,
                Service {
                    service_id: 1,
                    name: "svc1".to_string(),
                    transport: Transport::TCP,
                    host: "site1".to_string(),
                    port: 100,
                },
            ),
            (
                2,
                Service {
                    service_id: 2,
                    name: "svc2".to_string(),
                    transport: Transport::TCP,
                    host: "site2".to_string(),
                    port: 200,
                },
            ),
            (
                3,
                Service {
                    service_id: 3,
                    name: "svc3".to_string(),
                    transport: Transport::UDP,
                    host: "site3".to_string(),
                    port: 300,
                },
            ),
        ]);

        assert_eq!(
            actual_services
                .iter()
                .filter(|entry| !expected_access_db_map.contains_key(&entry.service_id))
                .count(),
            0
        );
    }

    #[test]
    fn inmemsvcrepo_delete_when_invalid_service() {
        let service_repo = InMemServiceRepo::new();
        let service_key = 1;
        let service = Service {
            service_id: 1,
            name: "svc1".to_string(),
            transport: Transport::TCP,
            host: "site1".to_string(),
            port: 100,
        };

        service_repo
            .services
            .write()
            .unwrap()
            .insert(service_key, service);

        let result = service_repo.delete(10);

        if let Err(err) = &result {
            panic!("Unexpected result: err={:?}", &err)
        }

        assert!(result.unwrap().is_none());
    }

    #[test]
    fn inmemsvcrepo_delete_when_valid_service() {
        let service_repo = InMemServiceRepo::new();
        let service_key = 1;
        let service = Service {
            service_id: 1,
            name: "svc1".to_string(),
            transport: Transport::TCP,
            host: "site1".to_string(),
            port: 100,
        };

        service_repo
            .services
            .write()
            .unwrap()
            .insert(service_key, service.clone());

        let result = service_repo.delete(1);

        if let Err(err) = &result {
            panic!("Unexpected result: err={:?}", &err)
        }

        let actual_prev_service = result.unwrap();

        assert!(actual_prev_service.is_some());
        assert_eq!(actual_prev_service.unwrap(), service);
    }
}
