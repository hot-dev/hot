// Shared Stream Graph Rendering
// This file contains the rendering logic for stream graphs across Run, Event, and Stream detail pages

// Global state for stream graph filtering
window.streamGraphState = {
    graphData: null,
    containerId: null,
    inspectorCallbackName: null,
    searchTerm: ''
};

function renderStreamGraph(containerId, graphDataJson, inspectorCallbackName = 'showStreamNodeInspector') {
    const graphContainer = document.getElementById(containerId);
    if (!graphContainer) {
        console.error('Stream graph container not found:', containerId);
        return;
    }

    // Parse graph data
    let graphData;
    try {
        if (typeof graphDataJson === 'string') {
            const cleanJson = graphDataJson.trim();
            if (!cleanJson || cleanJson === '{}') {
                graphContainer.innerHTML = '<div class="text-center text-gray-500 dark:text-gray-400 py-8">No graph data available</div>';
                return;
            }
            graphData = JSON.parse(cleanJson);
        } else {
            graphData = graphDataJson;
        }
    } catch (e) {
        console.error('Error parsing stream graph data:', e);
        graphContainer.innerHTML = '<div class="text-center text-red-500 dark:text-red-500 py-8">Error loading graph data</div>';
        return;
    }

    if (!graphData.nodes || graphData.nodes.length === 0) {
        graphContainer.innerHTML = '<div class="text-center text-gray-500 dark:text-gray-400 py-8">No graph data available</div>';
        return;
    }

    // Store state for re-rendering with filters
    window.streamGraphState.graphData = graphData;
    window.streamGraphState.containerId = containerId;
    window.streamGraphState.inspectorCallbackName = inspectorCallbackName;

    // Render as flat timeline-style list sorted by ID (UUIDv7) with git-style graph
    renderSimpleStreamGraph(graphContainer, graphData, inspectorCallbackName, window.streamGraphState.searchTerm);
}

// Filter and re-render the stream graph
function filterStreamGraph(searchTerm) {
    window.streamGraphState.searchTerm = (searchTerm || '').toLowerCase();

    const { graphData, containerId, inspectorCallbackName } = window.streamGraphState;
    if (!graphData || !containerId) return;

    const graphContainer = document.getElementById(containerId);
    if (!graphContainer) return;

    renderSimpleStreamGraph(graphContainer, graphData, inspectorCallbackName, window.streamGraphState.searchTerm);
}

// Check if a node matches the search term
function nodeMatchesSearch(node, details, searchTerm) {
    if (!searchTerm) return true;

    const nodeType = (node.node_type || '').toLowerCase();
    const typeValue = (details['TYPE'] || '').toLowerCase();
    const fnValue = (details['FN'] || '').toLowerCase();
    const statusValue = (details['STATUS'] || '').toLowerCase();
    const idValue = (node.id || '').toLowerCase();
    const resultValue = (node.result || '').toLowerCase();

    // Check all searchable fields
    if (nodeType.includes(searchTerm)) return true;
    if (typeValue.includes(searchTerm)) return true;
    if (fnValue.includes(searchTerm)) return true;
    if (statusValue.includes(searchTerm)) return true;
    if (idValue.includes(searchTerm)) return true;
    if (resultValue.includes(searchTerm)) return true;

    // Also check the full name (which contains all details)
    if ((node.name || '').toLowerCase().includes(searchTerm)) return true;

    return false;
}

function renderSimpleStreamGraph(container, graphData, inspectorCallbackName, searchTerm = '') {
    const nodes = graphData.nodes;
    const edges = graphData.edges || [];

    if (!nodes || nodes.length === 0) {
        container.innerHTML = '<div class="text-center text-gray-500 dark:text-gray-400 py-8">No nodes to display</div>';
        return;
    }

    // Build parent-child maps from edges
    const parentMap = new Map();  // child -> parent
    const childrenMap = new Map(); // parent -> [children]

    edges.forEach(edge => {
        const fromNode = edge.from || edge.source;
        const toNode = edge.to || edge.target;
        parentMap.set(toNode, fromNode);

        if (!childrenMap.has(fromNode)) {
            childrenMap.set(fromNode, []);
        }
        childrenMap.get(fromNode).push(toNode);
    });

    // Use the order provided by the backend (already sorted by timestamp)
    const sortedNodes = [...nodes];

    // Create a map for quick node lookup and position
    const nodeMap = new Map();
    const nodePositions = new Map();
    nodes.forEach(node => nodeMap.set(node.id, node));
    sortedNodes.forEach((node, idx) => nodePositions.set(node.id, idx));

    // Pre-parse details for filtering
    const nodeDetails = new Map();
    sortedNodes.forEach(node => {
        const details = {};
        const nameParts = (node.name || '').split('\n');
        for (let i = 1; i < nameParts.length; i++) {
            const part = nameParts[i];
            const colonIndex = part.indexOf(':');
            if (colonIndex > 0) {
                const key = part.substring(0, colonIndex).trim();
                const value = part.substring(colonIndex + 1).trim();
                details[key] = value;
            }
        }
        nodeDetails.set(node.id, details);
    });

    // Filter nodes based on search
    const filteredNodes = searchTerm
        ? sortedNodes.filter(node => nodeMatchesSearch(node, nodeDetails.get(node.id), searchTerm))
        : sortedNodes;

    // Calculate graph lines - track active "lanes" for git-style visualization
    // Each lane represents an ongoing connection from a parent to its children
    const graphLines = calculateGraphLines(sortedNodes, parentMap, childrenMap, nodePositions);

    // Build a set of filtered node IDs for quick lookup
    const filteredNodeIds = new Set(filteredNodes.map(n => n.id));

    // Render the table with graph column
    let html = `
        <div class="stream-tree">
            <table class="w-full text-xs border-collapse">
                <thead>
                    <tr class="text-left text-gray-500 dark:text-gray-400 uppercase text-[10px] tracking-wider" style="height: 32px;">
                        <th class="pl-2 pr-0 font-medium" style="width: 50px;"></th>
                        <th class="pl-2 pr-0 font-medium" colspan="2" style="width: 94px;">Type</th>
                        <th class="px-2 font-medium" style="width: 90px;">ID</th>
                        <th class="px-2 font-medium" style="width: 90px;">Kind</th>
                        <th class="px-2 font-medium">Function</th>
                        <th class="px-2 font-medium text-right" style="width: 80px;">Status</th>
                    </tr>
                </thead>
                <tbody>
    `;

    let visibleCount = 0;
    sortedNodes.forEach((node, idx) => {
        if (!filteredNodeIds.has(node.id)) return;
        visibleCount++;
        const lineInfo = graphLines[idx];
        html += renderNodeRow(node, lineInfo, inspectorCallbackName);
    });

    // Show message if all filtered out
    if (visibleCount === 0 && nodes.length > 0) {
        html += `
            <tr>
                <td colspan="7" class="text-center text-gray-500 dark:text-gray-400 py-8">
                    No matches found. ${nodes.length} items hidden by search.
                </td>
            </tr>
        `;
    }

    html += `
                </tbody>
            </table>
        </div>
    `;

    container.innerHTML = html;

    // Apply syntax highlighting to FN values
    if (typeof Prism !== 'undefined') {
        container.querySelectorAll('code.language-hot').forEach(el => {
            Prism.highlightElement(el);
        });
    }
}

function calculateGraphLines(sortedNodes, parentMap, childrenMap, nodePositions) {
    const lines = [];
    const activeLanes = new Map(); // nodeId -> lane index (for nodes with pending children)
    const availableLanes = []; // Pool of recycled lane indices
    const maxLanes = 3; // Limit to prevent overflow

    sortedNodes.forEach((node, idx) => {
        const parentId = parentMap.get(node.id);
        const children = childrenMap.get(node.id) || [];
        const hasChildren = children.length > 0;

        // Find remaining children for each active lane
        const activeNodeIds = Array.from(activeLanes.keys());

        // Check which lanes are still active (have children after this position)
        const stillActiveLanes = [];
        const lanesToRecycle = [];

        activeNodeIds.forEach(activeNodeId => {
            const activeChildren = childrenMap.get(activeNodeId) || [];
            const hasLaterChildren = activeChildren.some(childId => {
                const childPos = nodePositions.get(childId);
                return childPos !== undefined && childPos > idx;
            });
            if (hasLaterChildren) {
                stillActiveLanes.push({ nodeId: activeNodeId, lane: activeLanes.get(activeNodeId) });
            } else if (activeNodeId !== parentId) {
                // This lane is done and can be recycled (unless it's our parent's lane which we handle below)
                lanesToRecycle.push(activeLanes.get(activeNodeId));
                activeLanes.delete(activeNodeId);
            }
        });

        // Recycle completed lanes
        lanesToRecycle.forEach(lane => {
            if (!availableLanes.includes(lane)) {
                availableLanes.push(lane);
            }
        });
        availableLanes.sort((a, b) => a - b); // Prefer lower lane numbers

        // Determine this node's connection
        let connectionType = 'none';
        let parentLane = -1;
        let myLane = -1;

        if (parentId && activeLanes.has(parentId)) {
            parentLane = activeLanes.get(parentId);

            // Check if this is the last child of parent
            const parentChildren = childrenMap.get(parentId) || [];
            const laterSiblings = parentChildren.filter(childId => {
                const childPos = nodePositions.get(childId);
                return childPos !== undefined && childPos > idx;
            });

            if (laterSiblings.length === 0) {
                // Last child - end the lane and recycle it
                connectionType = 'end';
                const recycledLane = activeLanes.get(parentId);
                activeLanes.delete(parentId);
                if (!availableLanes.includes(recycledLane)) {
                    availableLanes.push(recycledLane);
                    availableLanes.sort((a, b) => a - b);
                }
            } else {
                // More siblings coming
                connectionType = 'through';
            }
        }

        // If this node has children, get a lane (recycle if possible)
        if (hasChildren) {
            if (availableLanes.length > 0) {
                myLane = availableLanes.shift(); // Reuse lowest available lane
            } else {
                // Allocate new lane, but cap at maxLanes
                const usedLanes = Array.from(activeLanes.values());
                const highestLane = usedLanes.length > 0 ? Math.max(...usedLanes) : -1;
                myLane = Math.min(highestLane + 1, maxLanes - 1);
            }
            activeLanes.set(node.id, myLane);

            if (connectionType === 'none') {
                connectionType = 'start';
            } else if (connectionType === 'end' || connectionType === 'through') {
                connectionType = 'branch';
            }
        }

        lines.push({
            connectionType,
            parentLane,
            myLane,
            activeLanes: stillActiveLanes.map(l => l.lane).filter(l => l < maxLanes),
            nodeType: (node.node_type || '').replace('current_', '')
        });
    });

    return lines;
}

function renderGraphCell(lineInfo) {
    const width = 50;
    const height = 40; // Taller to cover row padding
    const laneWidth = 12;
    const centerY = height / 2;
    const dotRadius = 3;
    const strokeWidth = 1.5;

    // Colors based on node type
    const nodeColor = lineInfo.nodeType === 'event'
        ? 'rgb(34, 197, 94)' // green-500
        : lineInfo.nodeType === 'task'
            ? 'rgb(245, 158, 11)' // amber-500
            : 'rgb(59, 130, 246)'; // blue-500

    const lineColor = 'rgb(156, 163, 175)'; // gray-400

    let svg = `<svg width="${width}" height="${height}" class="block">`;

    // Draw vertical lines for active lanes (full height to connect between rows)
    lineInfo.activeLanes.forEach(lane => {
        const x = 6 + lane * laneWidth;
        svg += `<line x1="${x}" y1="0" x2="${x}" y2="${height}" stroke="${lineColor}" stroke-width="${strokeWidth}"/>`;
    });

    // Draw connection based on type
    const myX = lineInfo.myLane >= 0 ? 6 + lineInfo.myLane * laneWidth : 6;
    const parentX = lineInfo.parentLane >= 0 ? 6 + lineInfo.parentLane * laneWidth : 6;

    switch (lineInfo.connectionType) {
        case 'start':
            // Start of new branch - dot with line going down
            svg += `<line x1="${myX}" y1="${centerY}" x2="${myX}" y2="${height}" stroke="${nodeColor}" stroke-width="${strokeWidth}"/>`;
            svg += `<circle cx="${myX}" cy="${centerY}" r="${dotRadius}" fill="${nodeColor}"/>`;
            break;

        case 'end':
            // End of branch - curved line from parent lane to dot
            if (parentX !== myX) {
                // Curved connection from parent lane
                svg += `<path d="M${parentX},0 L${parentX},${centerY - 4} Q${parentX},${centerY} ${parentX + 4},${centerY} L${myX},${centerY}"
                         fill="none" stroke="${lineColor}" stroke-width="${strokeWidth}"/>`;
            } else {
                svg += `<line x1="${parentX}" y1="0" x2="${parentX}" y2="${centerY}" stroke="${lineColor}" stroke-width="${strokeWidth}"/>`;
            }
            svg += `<circle cx="${myX}" cy="${centerY}" r="${dotRadius}" fill="${nodeColor}"/>`;
            break;

        case 'through':
            // Continuing through - vertical line continues, branch out to dot
            svg += `<line x1="${parentX}" y1="0" x2="${parentX}" y2="${height}" stroke="${lineColor}" stroke-width="${strokeWidth}"/>`;
            // Curved branch to the right
            const branchX = parentX + 14;
            svg += `<path d="M${parentX},${centerY} Q${parentX + 6},${centerY} ${branchX},${centerY}"
                     fill="none" stroke="${lineColor}" stroke-width="${strokeWidth}"/>`;
            svg += `<circle cx="${branchX}" cy="${centerY}" r="${dotRadius}" fill="${nodeColor}"/>`;
            break;

        case 'branch':
            // Both receiving from parent and starting new branch
            if (parentX !== myX) {
                svg += `<path d="M${parentX},0 L${parentX},${centerY - 4} Q${parentX},${centerY} ${parentX + 4},${centerY} L${myX},${centerY}"
                         fill="none" stroke="${lineColor}" stroke-width="${strokeWidth}"/>`;
            } else {
                svg += `<line x1="${parentX}" y1="0" x2="${parentX}" y2="${centerY}" stroke="${lineColor}" stroke-width="${strokeWidth}"/>`;
            }
            svg += `<line x1="${myX}" y1="${centerY}" x2="${myX}" y2="${height}" stroke="${nodeColor}" stroke-width="${strokeWidth}"/>`;
            svg += `<circle cx="${myX}" cy="${centerY}" r="${dotRadius}" fill="${nodeColor}"/>`;
            break;

        case 'none':
        default:
            // No connection - just a dot
            svg += `<circle cx="6" cy="${centerY}" r="${dotRadius}" fill="${nodeColor}"/>`;
            break;
    }

    svg += '</svg>';
    return svg;
}

// Format duration in microseconds to a human-readable string
function formatQueueWait(us) {
    if (us < 1000) {
        return `${us}μs`;
    } else if (us < 1000000) {
        return `${(us / 1000).toFixed(1)}ms`;
    } else {
        return `${(us / 1000000).toFixed(1)}s`;
    }
}

function renderNodeRow(node, lineInfo, inspectorCallbackName) {
    // Determine the actual node type
    const rawNodeType = node.node_type;
    const isCurrent = node.is_current || rawNodeType === 'current_run' || rawNodeType === 'current_event';
    const nodeType = rawNodeType.replace('current_', '');

    // Parse the name field
    const nameParts = node.name.split('\n');
    const nodeTypeLabel = nameParts[0] || nodeType.toUpperCase();

    // Build key-value pairs from remaining lines
    const details = {};
    for (let i = 1; i < nameParts.length; i++) {
        const part = nameParts[i];
        const colonIndex = part.indexOf(':');
        if (colonIndex > 0) {
            const key = part.substring(0, colonIndex).trim();
            const value = part.substring(colonIndex + 1).trim();
            details[key] = value;
        }
    }

    const shortId = details['ID'] || node.id.substring(0, 10);
    const fnValue = details['FN'] || '-';
    const typeValue = details['TYPE'] || nodeType;
    const statusValue = details['STATUS'] || '';

    // Determine row styling
    const navNodeType = nodeType === 'event' ? 'event' : nodeType === 'task' ? 'task' : 'run';
    const typeColor = nodeType === 'event'
        ? 'text-green-700 dark:text-green-400'
        : nodeType === 'task'
            ? 'text-amber-700 dark:text-amber-400'
            : 'text-blue-700 dark:text-blue-400';

    // FN display
    const fnDisplay = fnValue !== '-'
        ? `<code class="language-hot">${escapeHtml(fnValue)}</code>`
        : '<span class="text-gray-400 dark:text-gray-500 italic">-</span>';

    // Queue wait line (only for runs with queue wait time) - always on second line
    let queueWaitLine = '';
    if (nodeType === 'run' && node.queue_wait_us && node.queue_wait_us > 0) {
        const formattedWait = formatQueueWait(node.queue_wait_us);
        queueWaitLine = `<div class="text-[9px] text-yellow-600 dark:text-yellow-400 whitespace-nowrap" title="Queue wait time">⏱ ${formattedWait}</div>`;
    }

    // Status pill
    let statusHtml = '';
    if (statusValue && (nodeType === 'run' || nodeType === 'task')) {
        let statusClass;
        if (statusValue === 'succeeded' || statusValue === 'completed') {
            statusClass = nodeType === 'task'
                ? 'bg-amber-100 text-amber-800 dark:bg-amber-900 dark:text-amber-200'
                : 'bg-green-100 text-green-800 dark:bg-green-900 dark:text-green-200';
        } else if (statusValue === 'failed' || statusValue === 'timed_out') {
            statusClass = 'bg-red-100 text-red-800 dark:bg-red-900 dark:text-red-200';
        } else if (statusValue === 'running') {
            statusClass = 'bg-amber-100 text-amber-800 dark:bg-amber-900 dark:text-amber-200';
        } else {
            statusClass = 'bg-gray-100 text-gray-800 dark:bg-gray-800 dark:text-gray-200';
        }
        statusHtml = `<span class="px-1.5 py-0.5 text-[10px] font-medium rounded ${statusClass}">${escapeHtml(statusValue)}</span>`;
    }

    // Current badge
    const currentBadge = isCurrent
        ? '<span class="text-hot-red-500 text-[10px] font-bold ml-1">CURRENT</span>'
        : '';

    const graphCell = renderGraphCell(lineInfo);

    // Build link to detail page
    const detailUrl = nodeType === 'event' ? `/events/${node.id}`
        : nodeType === 'task' ? `/tasks/${node.id}`
        : `/runs/${node.id}`;

    return `
        <tr class="group hover:bg-gray-100 dark:hover:bg-[#222] cursor-pointer border-b border-gray-100 dark:border-neutral-800 transition-colors"
            style="height: 40px;"
            onclick="${navNodeType === 'task' ? `window.location.href='${detailUrl}'` : `window.${inspectorCallbackName}('${node.id}', '${navNodeType}', ${JSON.stringify(details).replace(/"/g, '&quot;')})`}">
            <td class="p-0 pl-2 align-middle" style="line-height: 0;">${graphCell}</td>
            <td class="pl-2 pr-0 align-middle" style="width: 24px;">${nodeTypeIcon(navNodeType, typeColor)}</td>
            <td class="px-2 align-middle">
                <div class="flex flex-col leading-tight">
                    <div><span class="font-semibold ${typeColor}">${escapeHtml(nodeTypeLabel)}</span>${currentBadge}</div>${queueWaitLine}
                </div>
            </td>
            <td class="px-2 align-middle font-mono">
                <span class="inline-flex items-center gap-1">
                    <a href="${detailUrl}" class="text-blue-600 dark:text-blue-400 hover:text-blue-700 dark:hover:text-blue-300 transition-all" onclick="event.stopPropagation()">${escapeHtml(shortId)}</a>
                    <button class="uuid-copy-btn text-gray-400 hover:text-gray-700 dark:text-gray-500 dark:hover:text-gray-200 p-0.5" data-uuid="${node.id}" title="Copy ${node.id} to clipboard" onclick="event.stopPropagation()">
                        <svg class="w-3 h-3" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2m-6 12h8a2 2 0 002-2v-8a2 2 0 00-2-2h-8a2 2 0 00-2 2v8a2 2 0 002 2z"></path></svg>
                    </button>
                </span>
            </td>
            <td class="px-2 align-middle">
                <span class="px-1.5 py-0.5 text-[10px] font-medium rounded bg-gray-100 text-gray-700 dark:bg-gray-800 dark:text-gray-300">${escapeHtml(typeValue)}</span>
            </td>
            <td class="px-2 align-middle text-gray-800 dark:text-gray-200 truncate max-w-xs">${fnDisplay}</td>
            <td class="px-2 align-middle text-right">${statusHtml}</td>
        </tr>
    `;
}

// Entity icon matching the main nav / detail-page tabs (run = play, event = sparkles, task = play-in-square)
function nodeTypeIcon(navNodeType, colorClass) {
    const inner = navNodeType === 'event'
        ? '<path stroke-linecap="round" stroke-linejoin="round" d="M9.813 15.904L9 18.75l-.813-2.846a4.5 4.5 0 00-3.09-3.09L2.25 12l2.846-.813a4.5 4.5 0 003.09-3.09L9 5.25l.813 2.846a4.5 4.5 0 003.09 3.09L15.75 12l-2.846.813a4.5 4.5 0 00-3.09 3.09zM18.259 8.715L18 9.75l-.259-1.035a3.375 3.375 0 00-2.455-2.456L14.25 6l1.036-.259a3.375 3.375 0 002.455-2.456L18 2.25l.259 1.035a3.375 3.375 0 002.456 2.456L21.75 6l-1.035.259a3.375 3.375 0 00-2.456 2.456z"/>'
        : navNodeType === 'task'
            ? '<rect x="3" y="3" width="18" height="18" rx="3" stroke-linecap="round" stroke-linejoin="round"/><path stroke-linecap="round" stroke-linejoin="round" d="M9.5 7.5v9l7.5-4.5-7.5-4.5z"/>'
            : '<path stroke-linecap="round" stroke-linejoin="round" d="M5.25 5.653c0-.856.917-1.398 1.667-.986l11.54 6.348a1.125 1.125 0 010 1.971l-11.54 6.347a1.125 1.125 0 01-1.667-.985V5.653z"/>';
    return `<svg class="w-3.5 h-3.5 flex-shrink-0 ${colorClass}" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 24 24" aria-hidden="true">${inner}</svg>`;
}

function escapeHtml(text) {
    const div = document.createElement('div');
    div.textContent = text;
    return div.innerHTML;
}

// Expose as global functions
window.renderStreamGraph = renderStreamGraph;
window.filterStreamGraph = filterStreamGraph;
