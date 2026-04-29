use ahash::AHashMap;
use chrono::Utc;
use croner::Cron;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Type alias for the complex async task function
type AsyncTask =
    Arc<dyn Fn(Uuid, Arc<Mutex<()>>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Represents a scheduled job that can be executed
pub struct Job {
    pub id: Uuid,
    pub cron_expression: String,
    pub cron: Cron,
    pub task: AsyncTask,
}

/// Custom scheduler that uses croner directly for full feature support
#[derive(Clone)]
pub struct JobScheduler {
    jobs: Arc<RwLock<AHashMap<Uuid, Job>>>,
    running: Arc<RwLock<bool>>,
    scheduler_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
}

impl JobScheduler {
    /// Create a new scheduler
    pub async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(JobScheduler {
            jobs: Arc::new(RwLock::new(AHashMap::new())),
            running: Arc::new(RwLock::new(false)),
            scheduler_handle: Arc::new(RwLock::new(None)),
        })
    }

    /// Add a job to the scheduler
    pub async fn add(&self, job: Job) -> Result<Uuid, Box<dyn std::error::Error + Send + Sync>> {
        let job_id = job.id;
        let mut jobs = self.jobs.write().await;
        jobs.insert(job_id, job);

        debug!(
            "Added job {} with cron expression: {}",
            job_id,
            jobs.get(&job_id).unwrap().cron_expression
        );

        Ok(job_id)
    }

    /// Remove a job from the scheduler
    pub async fn remove(
        &self,
        job_id: &Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut jobs = self.jobs.write().await;
        if jobs.remove(job_id).is_some() {
            info!("Removed job {}", job_id);
            Ok(())
        } else {
            warn!("Job {} not found for removal", job_id);
            Err(format!("Job {} not found", job_id).into())
        }
    }

    /// Start the scheduler
    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut running = self.running.write().await;
        if *running {
            return Ok(()); // Already running
        }
        *running = true;

        let jobs = Arc::clone(&self.jobs);
        let running_flag = Arc::clone(&self.running);

        let handle = tokio::spawn(async move {
            debug!("Starting custom croner-based scheduler");

            while *running_flag.read().await {
                let now = Utc::now();
                let jobs_read = jobs.read().await;

                for (job_id, job) in jobs_read.iter() {
                    // Check if this job should run now
                    if job.cron.is_time_matching(&now).unwrap_or(false) {
                        // Clone data needed for the spawned task
                        let lock = Arc::new(Mutex::new(()));
                        let task = Arc::clone(&job.task);
                        let job_id_clone = *job_id;

                        debug!("Executing job {} at {}", job_id, now);

                        // Execute the task
                        tokio::spawn(async move {
                            task(job_id_clone, lock).await;
                        });
                    }
                }

                drop(jobs_read);

                // Sleep for a short interval before checking again
                // Note: For production, you might want to calculate the exact time
                // until the next potential execution to be more efficient
                sleep(Duration::from_secs(1)).await;
            }

            info!("Custom scheduler stopped");
        });

        let mut scheduler_handle = self.scheduler_handle.write().await;
        *scheduler_handle = Some(handle);

        Ok(())
    }

    /// Stop the scheduler
    pub async fn shutdown(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut running = self.running.write().await;
        if !*running {
            return Ok(()); // Already stopped
        }
        *running = false;

        let mut handle_guard = self.scheduler_handle.write().await;
        if let Some(handle) = handle_guard.take() {
            handle.abort();
            info!("Scheduler shutdown complete");
        }

        Ok(())
    }
}

impl Job {
    /// Create a new async job with full croner support
    pub fn new_async<F, Fut>(
        cron_expression: &str,
        task: F,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>>
    where
        F: Fn(Uuid, Arc<Mutex<()>>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        // Parse with full croner capabilities - no restrictions!
        let cron = Cron::from_str(cron_expression)
            .map_err(|e| format!("Invalid cron expression '{}': {}", cron_expression, e))?;

        let job_id = Uuid::now_v7();

        let task_boxed = move |job_id: Uuid, lock: Arc<Mutex<()>>| {
            let fut = task(job_id, lock);
            Box::pin(fut) as Pin<Box<dyn Future<Output = ()> + Send>>
        };

        Ok(Job {
            id: job_id,
            cron_expression: cron_expression.to_string(),
            cron,
            task: Arc::new(task_boxed),
        })
    }

    /// Create a new job (non-async version)
    pub fn new<F>(
        cron_expression: &str,
        task: F,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>>
    where
        F: Fn(Uuid, Arc<Mutex<()>>) + Send + Sync + 'static,
    {
        Self::new_async(cron_expression, move |job_id, lock| {
            task(job_id, lock);
            async {}
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::time::{Duration, sleep};

    #[tokio::test]
    async fn test_scheduler_basic() {
        let mut scheduler = JobScheduler::new().await.unwrap();

        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = Arc::clone(&counter);

        // Create a job that runs every second
        let job = Job::new_async("* * * * * *", move |_job_id, _lock| {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                println!("Job executed! Count: {}", counter.load(Ordering::SeqCst));
            }
        })
        .unwrap();

        let job_id = scheduler.add(job).await.unwrap();
        scheduler.start().await.unwrap();

        // Wait a few seconds
        sleep(Duration::from_secs(3)).await;

        scheduler.remove(&job_id).await.unwrap();
        scheduler.shutdown().await.unwrap();

        // Should have executed at least once
        assert!(counter.load(Ordering::SeqCst) > 0);
    }

    #[tokio::test]
    async fn test_advanced_croner_features() {
        let mut scheduler = JobScheduler::new().await.unwrap();

        // Test advanced croner features that tokio-cron-scheduler doesn't support
        let advanced_expressions = vec![
            "@daily",          // Nickname
            "@weekly",         // Nickname
            "0 0 0 L * *",     // Last day of month
            "0 0 9 * * FRI#L", // Last Friday of month
            "0 0 9 * * MON#2", // Second Monday of month
            "0 0 9 15W * *",   // Closest weekday to 15th
            "0 0 12 1 * +MON", // 1st of month AND Monday
        ];

        for expr in advanced_expressions {
            let expr_clone = expr.to_string(); // Clone the expression for the closure
            let result = Job::new_async(expr, move |_job_id, _lock| {
                let expr = expr_clone.clone(); // Clone again for the async block
                async move {
                    println!("Advanced cron job executed: {}", expr);
                }
            });

            match result {
                Ok(job) => {
                    println!("✅ Successfully created job with expression: {}", expr);
                    let _job_id = scheduler.add(job).await.unwrap();
                }
                Err(e) => {
                    println!("❌ Failed to create job with expression '{}': {}", expr, e);
                }
            }
        }

        scheduler.shutdown().await.unwrap();
    }
}
