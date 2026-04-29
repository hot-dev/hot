use crate::templates;
use hot::db::{DatabasePool, Event, Run, Task};
use uuid::Uuid;

/// Focus element for highlighting in the stream graph
#[derive(Debug, Clone)]
pub enum FocusElement {
    Run(Uuid),
    Event(Uuid),
    Task(Uuid),
    None,
}

/// Build stream graph data with optional focus element highlighting
pub async fn build_stream_graph(
    db: &DatabasePool,
    stream_id: &Uuid,
    focus: FocusElement,
) -> templates::GraphNodeData {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let node_width = 420.0; // Must match frontend
    let node_height = 70.0; // Must match frontend
    let horizontal_spacing = 550.0; // Space between parallel nodes (more gap)
    let vertical_spacing = 150.0; // More vertical spacing

    tracing::debug!("Building stream graph for stream: {}", stream_id);

    // Get all runs and events for this stream
    let runs = Run::get_runs_by_stream(db, stream_id, None, None)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to fetch runs for stream graph: {}", e);
            Vec::new()
        });

    let events = Event::get_events_by_stream(db, stream_id, None, None)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to fetch events for stream graph: {}", e);
            Vec::new()
        });

    let tasks = Task::get_by_stream(db, stream_id, None)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to fetch tasks for stream graph: {}", e);
            Vec::new()
        });

    tracing::debug!(
        "Found {} runs, {} events, and {} tasks for stream graph",
        runs.len(),
        events.len(),
        tasks.len()
    );

    // Create a combined list of stream elements with timestamps
    #[derive(Debug)]
    enum StreamElement {
        Event(Event),
        Run(Box<Run>),
        Task(Box<Task>),
    }

    impl StreamElement {
        fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
            match self {
                StreamElement::Event(e) => e.created_at,
                StreamElement::Run(r) => r.start_time,
                StreamElement::Task(t) => t.created_at,
            }
        }

        fn id(&self) -> String {
            match self {
                StreamElement::Event(e) => e.event_id.to_string(),
                StreamElement::Run(r) => r.run_id.to_string(),
                StreamElement::Task(t) => t.task_id.to_string(),
            }
        }
    }

    let mut elements: Vec<StreamElement> = Vec::new();
    for event in events {
        elements.push(StreamElement::Event(event));
    }
    for run in runs {
        // Skip task-type runs — tasks are shown as their own nodes
        if run.run_type == "task" {
            continue;
        }
        elements.push(StreamElement::Run(Box::new(run)));
    }
    for task in tasks {
        elements.push(StreamElement::Task(Box::new(task)));
    }

    // Sort by timestamp (chronological order)
    elements.sort_by_key(|a| a.timestamp());

    tracing::debug!("Sorted {} total elements chronologically", elements.len());

    // Build nodes and edges with improved layout for parallel runs
    let mut prev_element_id: Option<String> = None;
    let mut y_position = 0.0;
    let mut i = 0;

    while i < elements.len() {
        let element = &elements[i];
        let element_id = element.id();

        match element {
            StreamElement::Event(event) => {
                // Check if this is the focused event
                let is_focused = matches!(&focus, FocusElement::Event(id) if *id == event.event_id);

                // Extract FN from event_data["fn"] if present
                let fn_display = event
                    .event_data
                    .get("fn")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "-".to_string());

                nodes.push(templates::GraphNode {
                    id: element_id.clone(),
                    name: format!(
                        "EVENT\nID: {}\nTYPE: {}\nFN: {}",
                        templates::get_uuid_short(&event.event_id),
                        event.event_type,
                        fn_display
                    ),
                    node_type: if is_focused {
                        "current_event".to_string()
                    } else {
                        "event".to_string()
                    },
                    status: None,
                    text_color: "#666666".to_string(),
                    x: -node_width / 2.0, // Center the node
                    result: None,         // Events don't have results
                    is_current: if is_focused { Some(true) } else { None },
                    y: y_position,
                    symbol_size: vec![node_width, node_height],
                    queue_wait_us: None, // Events don't have queue wait
                });

                // Add edge from previous element to current event
                if let Some(prev_id) = &prev_element_id {
                    edges.push(templates::GraphEdge {
                        source: prev_id.clone(),
                        target: element_id.clone(),
                        label: None,
                    });
                }

                prev_element_id = Some(element_id.clone());
                y_position += vertical_spacing;
                i += 1;

                // Check if next elements are runs triggered by this event
                let mut runs_for_this_event = Vec::new();
                let mut j = i;
                while j < elements.len() {
                    if let StreamElement::Run(run) = &elements[j] {
                        if run.event_id == Some(event.event_id) {
                            runs_for_this_event.push((j, run));
                            j += 1;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }

                // Layout runs triggered by this event
                if !runs_for_this_event.is_empty() {
                    let num_runs = runs_for_this_event.len();
                    let total_width = (num_runs - 1) as f64 * horizontal_spacing;
                    let start_x = -total_width / 2.0 - (node_width / 2.0); // Center accounting for node width

                    for (run_index, (_, run)) in runs_for_this_event.iter().enumerate() {
                        let run_element_id = run.run_id.to_string();
                        let x_position = start_x + (run_index as f64 * horizontal_spacing);

                        // Check if this is the focused run
                        let is_focused =
                            matches!(&focus, FocusElement::Run(id) if *id == run.run_id);

                        // For focused run, use special highlighting
                        let (status_color, node_type) = if is_focused {
                            ("#ef4444".to_string(), "current_run".to_string())
                        } else {
                            let color = match run.status.as_str() {
                                "succeeded" => "#22c55e",
                                "failed" => "#ef4444",
                                "cancelled" => "#6b7280",
                                _ => "#f59e0b",
                            };
                            (color.to_string(), "run".to_string())
                        };

                        let fn_display = run.event_fn.as_deref().unwrap_or("-");
                        let retry_suffix = if run.retry_attempt > 0 {
                            format!(" (retry #{})", run.retry_attempt)
                        } else {
                            String::new()
                        };

                        // Stringify result for search
                        let result_str = run
                            .result
                            .as_ref()
                            .map(|r| serde_json::to_string(r).unwrap_or_else(|_| r.to_string()));

                        // Calculate queue wait time if available
                        let queue_wait_us = run.queued_at.map(|queued_at| {
                            run.start_time
                                .signed_duration_since(queued_at)
                                .num_microseconds()
                                .unwrap_or(0)
                                .max(0) // Clamp to 0
                        });

                        nodes.push(templates::GraphNode {
                            id: run_element_id.clone(),
                            name: format!(
                                "RUN{}\nID: {}\nTYPE: {}\nFN: {}\nSTATUS: {}",
                                retry_suffix,
                                templates::get_uuid_short(&run.run_id),
                                run.run_type,
                                fn_display,
                                run.status
                            ),
                            node_type: node_type.clone(),
                            status: Some(run.status.clone()),
                            text_color: status_color,
                            x: x_position,
                            y: y_position,
                            symbol_size: vec![node_width, node_height],
                            result: result_str,
                            is_current: if node_type == "current_run" {
                                Some(true)
                            } else {
                                None
                            },
                            queue_wait_us,
                        });

                        // Add edge from event to run
                        edges.push(templates::GraphEdge {
                            source: element_id.clone(),
                            target: run_element_id.clone(),
                            label: None,
                        });
                    }

                    // Update prev_element_id to the first run for sequential flow
                    if let Some((_, first_run)) = runs_for_this_event.first() {
                        prev_element_id = Some(first_run.run_id.to_string());
                    }

                    y_position += vertical_spacing;
                    i += runs_for_this_event.len();
                }
            }
            StreamElement::Task(task) => {
                let status_str = &task.status;
                let is_focused = matches!(&focus, FocusElement::Task(id) if *id == task.task_id);

                let status_color = match status_str.as_str() {
                    "completed" => "#f59e0b", // amber-500
                    "failed" => "#ef4444",    // red-500
                    "timed_out" => "#ef4444",
                    "running" => "#f59e0b", // amber-500
                    _ => "#d97706",         // amber-600
                };

                let result_str = task
                    .result
                    .as_ref()
                    .map(|r| serde_json::to_string(r).unwrap_or_else(|_| r.to_string()));

                nodes.push(templates::GraphNode {
                    id: element_id.clone(),
                    name: format!(
                        "TASK\nID: {}\nFN: {}\nSTATUS: {}",
                        templates::get_uuid_short(&task.task_id),
                        task.function_name,
                        status_str
                    ),
                    node_type: if is_focused {
                        "current_task".to_string()
                    } else {
                        "task".to_string()
                    },
                    status: Some(status_str.clone()),
                    text_color: status_color.to_string(),
                    x: -node_width / 2.0,
                    y: y_position,
                    symbol_size: vec![node_width, node_height],
                    result: result_str,
                    is_current: if is_focused { Some(true) } else { None },
                    queue_wait_us: None,
                });

                // Prefer relationship-based edge: origin_run_id -> task
                if let Some(origin_id) = &task.origin_run_id {
                    let origin_str = origin_id.to_string();
                    // Only add edge if origin run node exists (it won't be a task-type run)
                    if nodes.iter().any(|n| n.id == origin_str) {
                        edges.push(templates::GraphEdge {
                            source: origin_str,
                            target: element_id.clone(),
                            label: None,
                        });
                    } else if let Some(prev_id) = &prev_element_id {
                        edges.push(templates::GraphEdge {
                            source: prev_id.clone(),
                            target: element_id.clone(),
                            label: None,
                        });
                    }
                } else if let Some(prev_id) = &prev_element_id {
                    edges.push(templates::GraphEdge {
                        source: prev_id.clone(),
                        target: element_id.clone(),
                        label: None,
                    });
                }

                prev_element_id = Some(element_id);
                y_position += vertical_spacing;
                i += 1;
            }
            StreamElement::Run(run) => {
                // This handles runs that aren't grouped with an event (e.g., orphaned runs)
                let is_focused = matches!(&focus, FocusElement::Run(id) if *id == run.run_id);

                let (status_color, node_type) = if is_focused {
                    ("#ef4444".to_string(), "current_run".to_string())
                } else {
                    let color = match run.status.as_str() {
                        "succeeded" => "#22c55e",
                        "failed" => "#ef4444",
                        "cancelled" => "#6b7280",
                        _ => "#f59e0b",
                    };
                    (color.to_string(), "run".to_string())
                };

                let fn_display = run.event_fn.as_deref().unwrap_or("-");
                let retry_suffix = if run.retry_attempt > 0 {
                    format!(" (retry #{})", run.retry_attempt)
                } else {
                    String::new()
                };

                // Stringify result for search
                let result_str = run
                    .result
                    .as_ref()
                    .map(|r| serde_json::to_string(r).unwrap_or_else(|_| r.to_string()));

                // Calculate queue wait time if available
                let queue_wait_us = run.queued_at.map(|queued_at| {
                    run.start_time
                        .signed_duration_since(queued_at)
                        .num_microseconds()
                        .unwrap_or(0)
                        .max(0) // Clamp to 0
                });

                nodes.push(templates::GraphNode {
                    id: element_id.clone(),
                    name: format!(
                        "RUN{}\nID: {}\nTYPE: {}\nFN: {}\nSTATUS: {}",
                        retry_suffix,
                        templates::get_uuid_short(&run.run_id),
                        run.run_type,
                        fn_display,
                        run.status
                    ),
                    node_type: node_type.clone(),
                    status: Some(run.status.clone()),
                    text_color: status_color,
                    x: -node_width / 2.0, // Center the node
                    y: y_position,
                    symbol_size: vec![node_width, node_height],
                    result: result_str,
                    is_current: if node_type == "current_run" {
                        Some(true)
                    } else {
                        None
                    },
                    queue_wait_us,
                });

                // Add edge from previous element to current run
                if let Some(prev_id) = &prev_element_id {
                    edges.push(templates::GraphEdge {
                        source: prev_id.clone(),
                        target: element_id.clone(),
                        label: None,
                    });
                }

                prev_element_id = Some(element_id);
                y_position += vertical_spacing;
                i += 1;
            }
        }
    }

    tracing::debug!(
        "Built stream graph with {} nodes and {} edges",
        nodes.len(),
        edges.len()
    );

    templates::GraphNodeData { nodes, edges }
}
