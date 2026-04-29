/**
 * Agent Graph — Custom HTML+SVG graph visualization for agent topology.
 *
 * Uses dagre for layout computation, renders HTML <div> nodes and SVG <path> edges.
 * Nodes: rectangular boxes with icons and text (GitHub Actions workflow style).
 * Edges: orthogonal step paths with arrowheads.
 * Layout: left-to-right DAG via dagre's layered algorithm.
 */
(function () {
  'use strict';

  var NODE_ICONS = {
    handler:    '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="ag-node-icon"><path stroke-linecap="round" stroke-linejoin="round" d="M5.25 5.653c0-.856.917-1.398 1.667-.986l11.54 6.348a1.125 1.125 0 010 1.971l-11.54 6.347a1.125 1.125 0 01-1.667-.985V5.653z" /></svg>',
    event_type: '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="ag-node-icon"><path stroke-linecap="round" stroke-linejoin="round" d="M9.813 15.904L9 18.75l-.813-2.846a4.5 4.5 0 00-3.09-3.09L2.25 12l2.846-.813a4.5 4.5 0 003.09-3.09L9 5.25l.813 2.846a4.5 4.5 0 003.09 3.09L15.75 12l-2.846.813a4.5 4.5 0 00-3.09 3.09zM18.259 8.715L18 9.75l-.259-1.035a3.375 3.375 0 00-2.455-2.456L14.25 6l1.036-.259a3.375 3.375 0 002.455-2.456L18 2.25l.259 1.035a3.375 3.375 0 002.456 2.456L21.75 6l-1.035.259a3.375 3.375 0 00-2.456 2.456z"/></svg>',
    schedule:   '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="ag-node-icon"><path stroke-linecap="round" stroke-linejoin="round" d="M12 6v6h4.5m4.5 0a9 9 0 1 1-18 0 9 9 0 0 1 18 0Z"/></svg>',
    webhook:    '<svg width="16" height="16" viewBox="20 150 980 830" fill="currentColor" class="ag-node-icon"><path d="M482 226h-1l-10 2q-33 4-64.5 18.5t-55.5 38.5q-41 37-57 91q-9 30-8 63t12 63q17 45 52 78l13 12l-83 135q-26-1-45 7q-30 13-45 40q-7 15-9 31t2 32q8 30 33 48q15 10 33 14.5t36 2t34.5-12.5t27.5-25q12-17 14.5-39t-5.5-41q-1-5-7-14l-3-6l118-192q6-9 8-14l-10-3q-9-2-13-4q-23-10-41.5-27.5t-28.5-39.5q-17-36-9-75q4-23 17-43t31-34q37-27 82-27q27-1 52.5 9.5t44.5 30.5q17 16 26.5 38.5t10.5 45.5q0 17-6 42l70 19l8 1q14-43 7-86q-4-33-19.5-63.5t-39.5-53.5q-42-42-103-56q-6-2-18-4l-14-2h-37zM500 350q-17 0-34 7t-30.5 20.5t-19.5 31.5q-8 20-4 44q3 18 14 34t28 25q24 15 56 13q3 4 5 8l112 191q3 6 6 9q27-26 58.5-35.5t65-3.5t58.5 26q32 25 43.5 61.5t0.5 73.5q-8 28-28.5 50t-48.5 33q-31 13-66.5 8.5t-63.5-24.5q-4-3-13-10l-5-6q-4 3-11 10l-47 46q23 23 52 38.5t61 21.5l22 4h39l28-5q64-13 110-60q22-22 36.5-50.5t19.5-59.5q5-36-2-71.5t-25-64.5t-44-51t-57-35q-34-14-70.5-16t-71.5 7l-17 5l-81-137q13-19 16-37q5-32-13-60q-16-25-44-35q-17-6-35-6zM218 614q-58 13-100 53q-47 44-61 105l-4 24v37l2 11q2 13 4 20q7 31 24.5 59t42.5 49q50 41 115 49q38 4 76-4.5t70-28.5q53-34 78-91q7-17 14-45q6-1 18 0l125 2q14 0 20 1q11 20 25 31t31.5 16t35.5 4q28-3 50-20q27-21 32-54q2-17-1.5-33t-13.5-30q-16-22-41-32q-17-7-35.5-6.5t-35.5 7.5q-28 12-43 37l-3 6q-14 0-42-1l-113-1q-15-1-43-1l-50-1l3 17q8 43-13 81q-14 27-40 45t-57 22q-35 6-70-7.5t-57-42.5q-28-35-27-79q1-37 23-69q13-19 32-32t41-19l9-3z"/></svg>',
    mcp_tool:   '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="ag-node-icon"><path stroke-linecap="round" stroke-linejoin="round" d="M11.42 15.17L17.25 21A2.652 2.652 0 0021 17.25l-5.877-5.877M11.42 15.17l2.496-3.03c.317-.384.74-.626 1.208-.766M11.42 15.17l-4.655 5.653a2.548 2.548 0 11-3.586-3.586l6.837-5.63m5.108-.233c.55-.164 1.163-.188 1.743-.14a4.5 4.5 0 004.486-6.336l-3.276 3.277a3.004 3.004 0 01-2.25-2.25l3.276-3.276a4.5 4.5 0 00-6.336 4.486c.091 1.076-.071 2.264-.904 2.95l-.102.085m-1.745 1.437L5.909 7.5H4.5L2.25 3.75l1.5-1.5L7.5 4.5v1.409l4.26 4.26m-1.745 1.437l1.745-1.437m6.615 8.206L15.75 15.75M4.867 19.125h.008v.008h-.008v-.008Z"/></svg>',
  };

  var EDGE_TYPES = {
    handles:  'ag-edge-handles',
    sends:    'ag-edge-sends',
    triggers: 'ag-edge-triggers',
  };

  function isIngressType(t) {
    return t === 'schedule' || t === 'webhook' || t === 'mcp_tool';
  }

  function esc(s) {
    if (!s) return '';
    var d = document.createElement('div');
    d.appendChild(document.createTextNode(s));
    return d.innerHTML;
  }

  // -----------------------------------------------------------------------
  // Node HTML builders
  // -----------------------------------------------------------------------

  function buildNodeHTML(node) {
    var icon = NODE_ICONS[node.node_type] || NODE_ICONS.event_type;
    var cls = 'ag-node ag-node-' + node.node_type;
    var name = node.name || '';
    if (name.length > 32) name = name.substring(0, 30) + '\u2026';

    var html = '<div class="' + cls + '" data-node-id="' + esc(node.id) + '">';
    html += '<span class="ag-node-icon-wrap">' + icon + '</span>';
    html += '<span class="ag-node-text">';
    html += '<span class="ag-node-name">' + esc(name) + '</span>';
    if (node.agent_name) {
      html += '<span class="ag-node-agent">' + esc(node.agent_name) + '</span>';
    }
    if (node.node_type === 'schedule' && node.detail) {
      html += '<span class="ag-node-detail">' + esc(node.detail) + '</span>';
    }
    html += '</span>';
    html += '</div>';
    return html;
  }

  // -----------------------------------------------------------------------
  // Edge SVG path builders (orthogonal step paths)
  // -----------------------------------------------------------------------

  function buildEdgePath(points) {
    if (!points || points.length < 2) return '';
    var d = 'M ' + points[0].x + ' ' + points[0].y;
    for (var i = 1; i < points.length; i++) {
      d += ' L ' + points[i].x + ' ' + points[i].y;
    }
    return d;
  }

  function orthogonalPath(src, tgt) {
    var midX = (src.x + tgt.x) / 2;
    return [
      { x: src.x, y: src.y },
      { x: midX, y: src.y },
      { x: midX, y: tgt.y },
      { x: tgt.x, y: tgt.y },
    ];
  }

  // -----------------------------------------------------------------------
  // Dagre layout
  // -----------------------------------------------------------------------

  function runLayout(data, direction) {
    var rankdir = direction || 'LR';
    var g = new dagre.graphlib.Graph();
    g.setGraph({
      rankdir: rankdir,
      nodesep: 20,
      ranksep: 70,
      edgesep: 10,
      marginx: 40,
      marginy: 40,
    });
    g.setDefaultEdgeLabel(function () { return {}; });

    var nodeMap = {};
    (data.nodes || []).forEach(function (n) {
      nodeMap[n.id] = n;
      var size = estimateNodeSize(n);
      g.setNode(n.id, { width: size[0], height: size[1], node: n });
    });

    (data.edges || []).forEach(function (e) {
      if (nodeMap[e.source] && nodeMap[e.target]) {
        g.setEdge(e.source, e.target, { edge: e });
      }
    });

    dagre.layout(g);
    return g;
  }

  function estimateNodeSize(node) {
    if (node.node_type === 'event_type') {
      var nameLen = (node.name || '').length;
      var w = Math.max(nameLen * 7.5 + 52, 100);
      return [Math.min(w, 220), 38];
    }
    var nameLen2 = (node.name || '').length;
    var agentLen = (node.agent_name || '').length;
    var textW = Math.max(nameLen2, agentLen) * 7.5 + 56;
    var w2 = Math.max(textW, 140);
    var h = 40;
    if (node.agent_name) h += 18;
    if (node.node_type === 'schedule' && node.detail) h += 16;
    return [Math.min(w2, 280), h];
  }

  // -----------------------------------------------------------------------
  // Renderer
  // -----------------------------------------------------------------------

  var MAX_CONTAINER_HEIGHT = 800;
  var MIN_CONTAINER_HEIGHT = 200;

  function render(container, data, direction) {
    container.innerHTML = '';
    container.classList.add('ag-container');

    var nodes = data.nodes || [];
    var edges = data.edges || [];
    if (!nodes.length) {
      container.innerHTML = '<div class="ag-empty">No graph data</div>';
      return;
    }

    var g = runLayout(data, direction);
    var graphLabel = g.graph();
    var graphW = graphLabel.width || 800;
    var graphH = graphLabel.height || 400;

    // Auto-size the container to fit the graph, capped at a max
    var maxH = parseInt(container.getAttribute('data-max-height'), 10) || MAX_CONTAINER_HEIGHT;
    var fitH = Math.max(Math.min(graphH, maxH), MIN_CONTAINER_HEIGHT);
    container.style.height = fitH + 'px';

    // Viewport wrapper — scrollable when graph exceeds container
    var viewport = document.createElement('div');
    viewport.className = 'ag-viewport';

    // Canvas holds nodes + SVG edges at the graph's natural size
    var canvas = document.createElement('div');
    canvas.className = 'ag-canvas';
    canvas.style.width = graphW + 'px';
    canvas.style.height = graphH + 'px';
    canvas.style.position = 'relative';

    // SVG layer for edges
    var svgNs = 'http://www.w3.org/2000/svg';
    var svg = document.createElementNS(svgNs, 'svg');
    svg.setAttribute('class', 'ag-edges-svg');
    svg.setAttribute('width', graphW);
    svg.setAttribute('height', graphH);
    svg.style.position = 'absolute';
    svg.style.left = '0';
    svg.style.top = '0';
    svg.style.pointerEvents = 'none';

    // Arrowhead markers
    var defs = document.createElementNS(svgNs, 'defs');
    ['handles', 'sends', 'triggers'].forEach(function (type) {
      var marker = document.createElementNS(svgNs, 'marker');
      marker.setAttribute('id', 'arrow-' + type);
      marker.setAttribute('viewBox', '0 0 10 10');
      marker.setAttribute('refX', '10');
      marker.setAttribute('refY', '5');
      marker.setAttribute('markerWidth', '5');
      marker.setAttribute('markerHeight', '5');
      marker.setAttribute('orient', 'auto-start-reverse');
      var path = document.createElementNS(svgNs, 'path');
      path.setAttribute('d', 'M 0 0 L 10 5 L 0 10 z');
      path.setAttribute('class', 'ag-arrow-' + type);
      marker.appendChild(path);
      defs.appendChild(marker);
    });
    svg.appendChild(defs);

    // Draw edges
    var edgeElements = {};
    g.edges().forEach(function (e) {
      var edgeData = g.edge(e);
      var rawEdge = edgeData.edge;
      if (!rawEdge) return;

      var points = edgeData.points;
      if (!points || points.length < 2) return;

      var pathStr = buildEdgePath(points);
      var pathEl = document.createElementNS(svgNs, 'path');
      pathEl.setAttribute('d', pathStr);
      pathEl.setAttribute('class', 'ag-edge ' + (EDGE_TYPES[rawEdge.edge_type] || 'ag-edge-handles'));
      pathEl.setAttribute('marker-end', 'url(#arrow-' + (rawEdge.edge_type || 'handles') + ')');
      pathEl.setAttribute('data-source', rawEdge.source);
      pathEl.setAttribute('data-target', rawEdge.target);
      pathEl.style.pointerEvents = 'stroke';

      svg.appendChild(pathEl);

      var key = rawEdge.source + '->' + rawEdge.target;
      edgeElements[key] = pathEl;
    });

    canvas.appendChild(svg);

    // Draw nodes
    var nodeElements = {};
    g.nodes().forEach(function (id) {
      var nodeData = g.node(id);
      if (!nodeData || !nodeData.node) return;

      var el = document.createElement('div');
      el.innerHTML = buildNodeHTML(nodeData.node);
      var nodeEl = el.firstChild;

      nodeEl.style.position = 'absolute';
      nodeEl.style.left = (nodeData.x - nodeData.width / 2) + 'px';
      nodeEl.style.top = (nodeData.y - nodeData.height / 2) + 'px';
      nodeEl.style.width = nodeData.width + 'px';

      canvas.appendChild(nodeEl);
      nodeElements[id] = nodeEl;
    });

    viewport.appendChild(canvas);
    container.appendChild(viewport);

    // Align canvas top-left (scroll if larger than viewport)

    return {
      nodeElements: nodeElements,
      edgeElements: edgeElements,
      canvas: canvas,
      viewport: viewport,
      svg: svg,
    };
  }

  // -----------------------------------------------------------------------
  // Tooltip
  // -----------------------------------------------------------------------


  function showTooltip(container, nodeEl, node) {
    hideTooltip(container);
    var tip = document.createElement('div');
    tip.className = 'ag-tooltip';

    var lines = [];
    if (node.node_type === 'handler') {
      lines.push('<strong>' + esc(node.name) + '</strong>');
      if (node.agent_name) lines.push('<span class="ag-tip-dim">Agent: ' + esc(node.agent_name) + '</span>');
      if (node.namespace) lines.push('<span class="ag-tip-mono">' + esc(node.namespace) + '</span>');
      if (node.description) lines.push(esc(node.description));
      if (node.retry) lines.push('<span class="ag-tip-dim">Retry: ' + esc(node.retry) + '</span>');
    } else if (node.node_type === 'schedule') {
      lines.push('<strong>Schedule</strong>');
      lines.push(esc(node.name));
      if (node.detail) lines.push('<span class="ag-tip-mono">' + esc(node.detail) + '</span>');
      if (node.active === false) lines.push('<span class="ag-tip-dim">Inactive</span>');
    } else if (node.node_type === 'webhook') {
      lines.push('<strong>Webhook</strong>');
      lines.push('<span class="ag-tip-mono">' + esc(node.name) + '</span>');
      if (node.detail) lines.push('<span class="ag-tip-mono">' + esc(node.detail) + '</span>');
    } else if (node.node_type === 'mcp_tool') {
      lines.push('<strong>MCP Tool</strong>');
      lines.push(esc(node.name));
      if (node.detail) lines.push('<span class="ag-tip-dim">Service: ' + esc(node.detail) + '</span>');
      if (node.description) lines.push('<em>' + esc(node.description) + '</em>');
    } else {
      lines.push('<strong>' + esc(node.name) + '</strong>');
      lines.push('<span class="ag-tip-dim">Event</span>');
    }
    tip.innerHTML = lines.join('<br>');

    var rect = nodeEl.getBoundingClientRect();
    var cRect = container.getBoundingClientRect();
    var gap = 8;
    var pad = 4;

    // Append first so we can measure the tooltip's actual size.
    tip.style.left = '0px';
    tip.style.top = '0px';
    tip.style.visibility = 'hidden';
    container.appendChild(tip);

    var tipRect = tip.getBoundingClientRect();
    var tipW = tipRect.width;
    var tipH = tipRect.height;
    var cW = cRect.width;
    var cH = cRect.height;

    // Prefer placing to the right of the node; flip to the left if it
    // would overflow the container's right edge.
    var leftPx = rect.right - cRect.left + gap;
    if (leftPx + tipW + pad > cW) {
      var flipped = rect.left - cRect.left - gap - tipW;
      leftPx = flipped >= pad ? flipped : Math.max(pad, cW - tipW - pad);
    }
    if (leftPx < pad) leftPx = pad;

    // Top-align with the node, then clamp into the container vertically.
    var topPx = rect.top - cRect.top;
    if (topPx + tipH + pad > cH) topPx = cH - tipH - pad;
    if (topPx < pad) topPx = pad;

    tip.style.left = leftPx + 'px';
    tip.style.top = topPx + 'px';
    tip.style.visibility = '';
  }

  function hideTooltip(container) {
    var existing = container.querySelector('.ag-tooltip');
    if (existing) existing.remove();
  }

  // -----------------------------------------------------------------------
  // Inspector helpers (same data format as before)
  // -----------------------------------------------------------------------

  function findConnectedEdges(graphData, nodeId) {
    var inbound = [], outbound = [];
    (graphData.edges || []).forEach(function (e) {
      if (e.target === nodeId) inbound.push(e);
      if (e.source === nodeId) outbound.push(e);
    });
    return { inbound: inbound, outbound: outbound };
  }

  function findNodeById(graphData, id) {
    return (graphData.nodes || []).find(function (n) { return n.id === id; }) || null;
  }

  function buildInspectorData(graphData, nodeId) {
    var node = findNodeById(graphData, nodeId);
    if (!node) return null;
    var connected = findConnectedEdges(graphData, nodeId);
    var info = {
      node: node,
      handledBy: [],
      sentBy: [],
      handles: [],
      sends: [],
      triggeredBy: [],
      triggers: [],
    };

    if (node.node_type === 'event_type') {
      connected.inbound.forEach(function (e) {
        if (e.edge_type === 'sends') {
          var src = findNodeById(graphData, e.source);
          if (src) info.sentBy.push(src);
        }
      });
      connected.outbound.forEach(function (e) {
        if (e.edge_type === 'handles') {
          var tgt = findNodeById(graphData, e.target);
          if (tgt) info.handledBy.push(tgt);
        }
      });
    } else if (node.node_type === 'handler') {
      connected.inbound.forEach(function (e) {
        if (e.edge_type === 'handles') {
          var src = findNodeById(graphData, e.source);
          if (src) info.handles.push({ node: src, edge: e });
        }
        if (e.edge_type === 'triggers') {
          var src2 = findNodeById(graphData, e.source);
          if (src2) info.triggeredBy.push({ node: src2, edge: e });
        }
      });
      connected.outbound.forEach(function (e) {
        if (e.edge_type === 'sends') {
          var tgt = findNodeById(graphData, e.target);
          if (tgt) info.sends.push({ node: tgt, edge: e });
        }
      });
    } else if (isIngressType(node.node_type)) {
      connected.outbound.forEach(function (e) {
        if (e.edge_type === 'triggers') {
          var tgt = findNodeById(graphData, e.target);
          if (tgt) info.triggers.push(tgt);
        }
      });
    }
    return info;
  }

  // -----------------------------------------------------------------------
  // Highlight helpers
  // -----------------------------------------------------------------------

  function highlightNode(inst, nodeId) {
    clearHighlight(inst);
    var connectedNodes = {};
    var connectedEdgeKeys = {};
    connectedNodes[nodeId] = true;

    (inst.graphData.edges || []).forEach(function (e) {
      if (e.source === nodeId || e.target === nodeId) {
        connectedNodes[e.source] = true;
        connectedNodes[e.target] = true;
        connectedEdgeKeys[e.source + '->' + e.target] = true;
      }
    });

    var selectedEl = null;
    Object.keys(inst.renderResult.nodeElements).forEach(function (id) {
      var el = inst.renderResult.nodeElements[id];
      if (!connectedNodes[id]) {
        el.classList.add('ag-dimmed');
      } else if (id === nodeId) {
        el.classList.add('ag-selected');
        selectedEl = el;
      } else {
        el.classList.add('ag-connected');
      }
    });

    Object.keys(inst.renderResult.edgeElements).forEach(function (key) {
      var el = inst.renderResult.edgeElements[key];
      if (!connectedEdgeKeys[key]) {
        el.classList.add('ag-dimmed');
      } else {
        el.classList.add('ag-highlighted');
      }
    });

    if (selectedEl) {
      setTimeout(function () {
        scrollHighlightedIntoView(inst, selectedEl);
      }, 120);
    }
  }

  function scrollHighlightedIntoView(inst, selectedEl) {
    var viewport = inst.el.querySelector('.ag-viewport');
    if (!viewport) return;

    var highlighted = viewport.querySelectorAll('.ag-selected, .ag-connected');
    if (!highlighted.length) {
      selectedEl.scrollIntoView({ behavior: 'smooth', block: 'nearest', inline: 'nearest' });
      return;
    }

    var vpRect = viewport.getBoundingClientRect();
    var PAD = 24;
    var SIZE_CAP = 1.5;

    var minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    highlighted.forEach(function (el) {
      var r = el.getBoundingClientRect();
      var x = r.left - vpRect.left + viewport.scrollLeft;
      var y = r.top - vpRect.top + viewport.scrollTop;
      if (x < minX) minX = x;
      if (y < minY) minY = y;
      if (x + r.width > maxX) maxX = x + r.width;
      if (y + r.height > maxY) maxY = y + r.height;
    });

    var boxW = maxX - minX;
    var boxH = maxY - minY;
    var visW = viewport.clientWidth;
    var visH = viewport.clientHeight;

    if (boxW > visW * SIZE_CAP || boxH > visH * SIZE_CAP) {
      var sr = selectedEl.getBoundingClientRect();
      var sx = sr.left - vpRect.left + viewport.scrollLeft;
      var sy = sr.top - vpRect.top + viewport.scrollTop;
      viewport.scrollTo({
        left: sx - (visW - sr.width) / 2,
        top: sy - (visH - sr.height) / 2,
        behavior: 'smooth'
      });
      return;
    }

    var targetLeft = minX - PAD;
    var targetTop = minY - PAD;
    var targetRight = maxX + PAD;
    var targetBottom = maxY + PAD;

    var scrollLeft = viewport.scrollLeft;
    var scrollTop = viewport.scrollTop;

    if (targetLeft < scrollLeft) {
      scrollLeft = targetLeft;
    } else if (targetRight > scrollLeft + visW) {
      scrollLeft = targetRight - visW;
    }

    if (targetTop < scrollTop) {
      scrollTop = targetTop;
    } else if (targetBottom > scrollTop + visH) {
      scrollTop = targetBottom - visH;
    }

    viewport.scrollTo({ left: scrollLeft, top: scrollTop, behavior: 'smooth' });
  }

  function nodeSearchText(node) {
    return [
      node.name,
      node.agent_name,
      node.namespace,
      node.qualified_name,
      node.project,
      node.description,
      node.detail,
      node.source_file,
      node.node_type,
    ].filter(Boolean).join(' ').toLowerCase();
  }

  function search(containerId, query) {
    var inst = instances[containerId];
    if (!inst || !inst.graphData || !inst.renderResult) return { matches: 0, index: 0 };

    var q = (query || '').trim().toLowerCase();
    inst.searchQuery = q;
    inst.searchMatches = [];
    inst.searchIndex = -1;
    clearHighlight(inst);
    if (!q) return searchStatus(inst);

    var matches = [];
    (inst.graphData.nodes || []).forEach(function (node) {
      if (nodeSearchText(node).indexOf(q) !== -1) {
        matches.push(node.id);
      }
    });

    inst.searchMatches = matches;
    inst.searchIndex = matches.length ? 0 : -1;
    renderSearchMatches(inst);

    return searchStatus(inst);
  }

  function searchStatus(inst) {
    return {
      matches: inst.searchMatches ? inst.searchMatches.length : 0,
      index: inst.searchIndex >= 0 ? inst.searchIndex + 1 : 0,
    };
  }

  function renderSearchMatches(inst) {
    clearHighlight(inst);
    if (!inst.searchMatches || !inst.searchMatches.length) return;

    var currentId = inst.searchMatches[inst.searchIndex];
    var matchSet = {};
    inst.searchMatches.forEach(function (id) {
      matchSet[id] = true;
    });

    Object.keys(inst.renderResult.nodeElements).forEach(function (id) {
      var el = inst.renderResult.nodeElements[id];
      if (id === currentId) {
        el.classList.add('ag-selected');
      } else if (matchSet[id]) {
        el.classList.add('ag-connected');
      } else {
        el.classList.add('ag-dimmed');
      }
    });
    Object.keys(inst.renderResult.edgeElements).forEach(function (key) {
      inst.renderResult.edgeElements[key].classList.add('ag-dimmed');
    });

    if (currentId) {
      var currentEl = inst.renderResult.nodeElements[currentId];
      if (currentEl) {
        setTimeout(function () {
          scrollHighlightedIntoView(inst, currentEl);
        }, 120);
      }
    }
  }

  function nextSearchMatch(containerId, delta) {
    var inst = instances[containerId];
    if (!inst || !inst.searchMatches || !inst.searchMatches.length) {
      return { matches: 0, index: 0 };
    }
    var len = inst.searchMatches.length;
    inst.searchIndex = (inst.searchIndex + delta + len) % len;
    renderSearchMatches(inst);
    return searchStatus(inst);
  }

  function clearHighlight(inst) {
    if (!inst.renderResult) return;
    Object.keys(inst.renderResult.nodeElements).forEach(function (id) {
      var el = inst.renderResult.nodeElements[id];
      el.classList.remove('ag-dimmed', 'ag-selected', 'ag-connected');
    });
    Object.keys(inst.renderResult.edgeElements).forEach(function (key) {
      var el = inst.renderResult.edgeElements[key];
      el.classList.remove('ag-dimmed', 'ag-highlighted');
    });
  }

  // -----------------------------------------------------------------------
  // Public API — same interface as before for template compatibility
  // -----------------------------------------------------------------------

  var instances = {};

  function init(containerId, opts) {
    opts = opts || {};
    var el = document.getElementById(containerId);
    if (!el) return;
    var url = el.getAttribute('data-url') || opts.url;
    if (!url) return;

    instances[containerId] = {
      el: el,
      url: url,
      direction: opts.direction || 'LR',
      graphData: null,
      renderResult: null,
      searchQuery: '',
      searchMatches: [],
      searchIndex: -1,
      chart: { resize: function () { /* compat stub */ } },
    };

    fetchAndRender(containerId);
  }

  function fetchAndRender(containerId) {
    var inst = instances[containerId];
    if (!inst) return;
    var url = inst.url;

    var filterEl = document.getElementById('agent-graph-filters');
    if (filterEl && filterEl.__x) {
      var state = filterEl.__x.$data;
      var params = [];
      if (state.workflow) params.push('workflow=' + encodeURIComponent(state.workflow));
      if (state.agent) params.push('agent=' + encodeURIComponent(state.agent));
      if (state.project) params.push('project=' + encodeURIComponent(state.project));
      if (state.tag) params.push('tag=' + encodeURIComponent(state.tag));
      if (params.length) url += '?' + params.join('&');
    }

    inst.el.innerHTML = '<div class="ag-loading">Loading\u2026</div>';

    fetch(url, { credentials: 'same-origin' })
      .then(function (res) { return res.json(); })
      .then(function (data) {
        inst.graphData = data;
        inst.renderResult = render(inst.el, data, inst.direction);
        if (inst.renderResult) {
          bindEvents(containerId);
          if (filterEl && filterEl.__x) {
            var result = search(containerId, filterEl.__x.$data.search);
            filterEl.__x.$data.searchMatches = result.matches;
            filterEl.__x.$data.searchIndex = result.index;
          }
        }
      })
      .catch(function (err) {
        inst.el.innerHTML = '<div class="ag-empty">Failed to load graph</div>';
        console.error('Agent graph fetch error:', err);
      });
  }

  function bindEvents(containerId) {
    var inst = instances[containerId];
    if (!inst || !inst.renderResult) return;

    // Node click → inspector
    Object.keys(inst.renderResult.nodeElements).forEach(function (id) {
      var nodeEl = inst.renderResult.nodeElements[id];
      nodeEl.addEventListener('click', function (evt) {
        evt.stopPropagation();
        var d = buildInspectorData(inst.graphData, id);
        if (d) {
          highlightNode(inst, id);
          dispatchInspector(containerId, d);
        }
      });

      // Hover tooltip
      nodeEl.addEventListener('mouseenter', function () {
        var node = findNodeById(inst.graphData, id);
        if (node) showTooltip(inst.el, nodeEl, node);
      });
      nodeEl.addEventListener('mouseleave', function () {
        hideTooltip(inst.el);
      });
    });

    // Click empty space → close inspector
    inst.el.addEventListener('click', function (evt) {
      if (evt.target === inst.el ||
          evt.target.classList.contains('ag-viewport') ||
          evt.target.classList.contains('ag-canvas')) {
        clearHighlight(inst);
        dispatchInspectorClose(containerId);
      }
    });
  }

  function dispatchInspector(cid, data) {
    document.dispatchEvent(new CustomEvent('agent-graph:inspect', { detail: { containerId: cid, data: data } }));
  }

  function dispatchInspectorClose(cid) {
    document.dispatchEvent(new CustomEvent('agent-graph:inspect-close', { detail: { containerId: cid } }));
  }

  function selectNode(containerId, nodeId) {
    var inst = instances[containerId];
    if (!inst || !inst.graphData) return;
    var d = buildInspectorData(inst.graphData, nodeId);
    if (d) {
      highlightNode(inst, nodeId);
      dispatchInspector(containerId, d);
    }
  }

  function refreshChart(containerId) {
    fetchAndRender(containerId);
  }

  function setDirection(containerId, dir) {
    var inst = instances[containerId];
    if (!inst) return;
    inst.direction = dir;
    if (inst.graphData) {
      inst.renderResult = render(inst.el, inst.graphData, inst.direction);
      if (inst.renderResult) {
        bindEvents(containerId);
        search(containerId, inst.searchQuery);
      }
    }
  }

  function getDirection(containerId) {
    var inst = instances[containerId];
    return inst ? inst.direction : 'LR';
  }

  window.AgentGraph = {
    init: init,
    refresh: refreshChart,
    fetchAndRender: fetchAndRender,
    selectNode: selectNode,
    search: search,
    nextSearchMatch: nextSearchMatch,
    setDirection: setDirection,
    getDirection: getDirection,
    _instances: instances,
  };
})();
