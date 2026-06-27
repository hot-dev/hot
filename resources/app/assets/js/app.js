/**
 * Hot App - Core JavaScript Utilities
 *
 * This file contains all core utilities used across the Hot app:
 * - Value Formatters (Hot literal and JSON formatting)
 * - Content Modal (view full values with format switching)
 * - UUID Utilities (truncation, clipboard copy)
 * - Sidebar Management (collapse/expand, mobile drawer)
 * - File Attachments (data URL detection, thumbnails, lightbox)
 */

(function() {
    'use strict';

    // ============================================
    // Value Formatters
    // ============================================

    /**
     * Check if a string is a valid Hot identifier (can be unquoted map key)
     */
    function isValidHotIdentifier(s) {
        if (!s || s.length === 0) return false;
        if (!/^[a-zA-Z_$]/.test(s)) return false;
        return /^[a-zA-Z_$][a-zA-Z0-9_$-]*$/.test(s);
    }

    /**
     * Format a value as a Hot literal (with types, unquoted keys when valid)
     */
    function formatAsHotLiteral(value, indent) {
        if (indent === undefined) indent = 0;

        if (value === null || value === undefined) {
            return 'null';
        }

        var indentStr = '  '.repeat(indent);
        var nextIndentStr = '  '.repeat(indent + 1);

        // String: wrap in double quotes and escape special characters
        if (typeof value === 'string') {
            return '"' + value
                .replace(/\\/g, '\\\\')
                .replace(/"/g, '\\"')
                .replace(/\n/g, '\\n')
                .replace(/\r/g, '\\r')
                .replace(/\t/g, '\\t')
                + '"';
        }

        // Boolean
        if (typeof value === 'boolean') {
            return value ? 'true' : 'false';
        }

        // Number
        if (typeof value === 'number') {
            return String(value);
        }

        // Array: format as Hot vector
        if (Array.isArray(value)) {
            if (value.length === 0) {
                return '[]';
            }
            var items = value.map(function(item) {
                return nextIndentStr + formatAsHotLiteral(item, indent + 1);
            });
            return '[\n' + items.join(',\n') + '\n' + indentStr + ']';
        }

        // Object: check for typed value first
        if (typeof value === 'object') {
            // Check for $type/$val pattern (typed Hot value)
            if (value['$type'] && value['$val'] !== undefined) {
                var typeStr = value['$type'];
                var valStr = formatAsHotLiteral(value['$val'], 0);
                return typeStr + '(' + valStr + ')';
            }

            var keys = Object.keys(value);
            if (keys.length === 0) {
                return '{}';
            }

            var entries = keys.map(function(key) {
                var keyStr = isValidHotIdentifier(key) ? key : formatAsHotLiteral(key, 0);
                var valStr = formatAsHotLiteral(value[key], indent + 1);
                return nextIndentStr + keyStr + ': ' + valStr;
            });
            return '{\n' + entries.join(',\n') + '\n' + indentStr + '}';
        }

        return String(value);
    }

    /**
     * Format a value as JSON (quoted keys, standard JSON structure)
     */
    function formatAsJson(value, indent) {
        if (indent === undefined) indent = 0;

        if (value === null || value === undefined) {
            return 'null';
        }

        var indentStr = '  '.repeat(indent);
        var nextIndentStr = '  '.repeat(indent + 1);

        if (typeof value === 'string') {
            return '"' + value
                .replace(/\\/g, '\\\\')
                .replace(/"/g, '\\"')
                .replace(/\n/g, '\\n')
                .replace(/\r/g, '\\r')
                .replace(/\t/g, '\\t')
                + '"';
        }

        if (typeof value === 'boolean') {
            return value ? 'true' : 'false';
        }

        if (typeof value === 'number') {
            return String(value);
        }

        if (Array.isArray(value)) {
            if (value.length === 0) {
                return '[]';
            }
            var items = value.map(function(item) {
                return nextIndentStr + formatAsJson(item, indent + 1);
            });
            return '[\n' + items.join(',\n') + '\n' + indentStr + ']';
        }

        if (typeof value === 'object') {
            var keys = Object.keys(value);
            if (keys.length === 0) {
                return '{}';
            }
            var entries = keys.map(function(key) {
                var keyStr = '"' + key.replace(/\\/g, '\\\\').replace(/"/g, '\\"') + '"';
                var valStr = formatAsJson(value[key], indent + 1);
                return nextIndentStr + keyStr + ': ' + valStr;
            });
            return '{\n' + entries.join(',\n') + '\n' + indentStr + '}';
        }

        return String(value);
    }

    // ============================================
    // Content Modal
    // ============================================

    // Current modal format (will be set from page context)
    var currentModalFormat = 'hot';

    /**
     * Initialize modal format from page context
     */
    function initModalFormat(format) {
        currentModalFormat = format || 'hot';
    }

    /**
     * Open content modal with optional raw data for format switching
     */
    function openContentModal(title, content, rawData) {
        var modal = document.getElementById('content-modal');
        var modalTitle = document.getElementById('content-modal-title');
        var modalBody = document.getElementById('content-modal-body');
        var formatToggle = document.getElementById('content-modal-format-toggle');
        var rawDataEl = document.getElementById('content-modal-raw-data');

        if (!modal || !modalTitle || !modalBody) return;

        modalTitle.textContent = title;

        // Store raw data if provided (for format switching)
        if (rawData) {
            if (rawDataEl) {
                rawDataEl.textContent = typeof rawData === 'string' ? rawData : JSON.stringify(rawData);
            }
            if (formatToggle) {
                formatToggle.classList.remove('hidden');
            }
            updateFormatButtons(currentModalFormat);
            // Display in current format
            var formatted = formatValueForModal(rawData, currentModalFormat);
            modalBody.textContent = formatted;
        } else {
            if (rawDataEl) {
                rawDataEl.textContent = '';
            }
            if (formatToggle) {
                formatToggle.classList.add('hidden');
            }
            // Display content as-is
            modalBody.textContent = content;
        }

        modal.classList.remove('hidden');
        document.body.style.overflow = 'hidden';

        // Apply syntax highlighting if Prism is available
        if (typeof Prism !== 'undefined') {
            modalBody.className = currentModalFormat === 'json'
                ? 'language-json text-sm whitespace-pre-wrap break-words'
                : 'language-hot text-sm whitespace-pre-wrap break-words';
            Prism.highlightElement(modalBody);
        }
    }

    /**
     * Open modal with pre-formatted Hot and JSON strings from backend
     */
    function openContentModalWithFormats(title, hotContent, jsonContent) {
        var modal = document.getElementById('content-modal');
        var modalTitle = document.getElementById('content-modal-title');
        var modalBody = document.getElementById('content-modal-body');
        var formatToggle = document.getElementById('content-modal-format-toggle');

        if (!modal || !modalTitle || !modalBody) return;

        modalTitle.textContent = title;

        // Decode HTML entities from script tags
        var decodedHotContent = decodeHtmlEntities(hotContent);
        var decodedJsonContent = decodeHtmlEntities(jsonContent);

        // Store both formats in data attributes for toggling
        modalBody.setAttribute('data-hot-content', decodedHotContent);
        modalBody.setAttribute('data-json-content', decodedJsonContent);

        // Show format toggle
        if (formatToggle) {
            formatToggle.classList.remove('hidden');
        }
        updateFormatButtons(currentModalFormat);

        // Display in current format
        var content = currentModalFormat === 'json' ? decodedJsonContent : decodedHotContent;
        modalBody.textContent = content;

        modal.classList.remove('hidden');
        document.body.style.overflow = 'hidden';

        // Apply syntax highlighting
        if (typeof Prism !== 'undefined') {
            modalBody.className = currentModalFormat === 'json'
                ? 'language-json text-sm whitespace-pre-wrap break-words'
                : 'language-hot text-sm whitespace-pre-wrap break-words';
            Prism.highlightElement(modalBody);
        }
    }

    /**
     * Switch modal format (unified handler)
     */
    function switchModalFormat(format) {
        var modalBody = document.getElementById('content-modal-body');
        if (!modalBody) return;

        var hotContent = modalBody.getAttribute('data-hot-content');
        var jsonContent = modalBody.getAttribute('data-json-content');

        if (hotContent && jsonContent) {
            setContentModalFormatPreformatted(format);
        } else {
            setContentModalFormat(format);
        }
    }

    /**
     * Set modal format using JS-based formatting
     */
    function setContentModalFormat(format) {
        currentModalFormat = format;
        var rawDataEl = document.getElementById('content-modal-raw-data');
        var modalBody = document.getElementById('content-modal-body');

        if (rawDataEl && rawDataEl.textContent) {
            try {
                var rawData = JSON.parse(rawDataEl.textContent);
                var formatted = formatValueForModal(rawData, format);
                modalBody.textContent = formatted;
                updateFormatButtons(format);

                if (typeof Prism !== 'undefined') {
                    modalBody.className = format === 'json'
                        ? 'language-json text-sm whitespace-pre-wrap break-words'
                        : 'language-hot text-sm whitespace-pre-wrap break-words';
                    Prism.highlightElement(modalBody);
                }
            } catch (e) {
                console.error('Failed to parse raw data for format switch:', e);
            }
        }
    }

    /**
     * Set modal format using pre-formatted content
     */
    function setContentModalFormatPreformatted(format) {
        currentModalFormat = format;
        var modalBody = document.getElementById('content-modal-body');
        if (!modalBody) return;

        var hotContent = modalBody.getAttribute('data-hot-content');
        var jsonContent = modalBody.getAttribute('data-json-content');

        if (hotContent && jsonContent) {
            var content = format === 'json' ? jsonContent : hotContent;
            modalBody.textContent = content;
            updateFormatButtons(format);

            if (typeof Prism !== 'undefined') {
                modalBody.className = format === 'json'
                    ? 'language-json text-sm whitespace-pre-wrap break-words'
                    : 'language-hot text-sm whitespace-pre-wrap break-words';
                Prism.highlightElement(modalBody);
            }
        } else {
            setContentModalFormat(format);
        }
    }

    /**
     * Update format button styles
     */
    function updateFormatButtons(activeFormat) {
        var hotBtn = document.getElementById('content-modal-format-hot');
        var jsonBtn = document.getElementById('content-modal-format-json');

        if (!hotBtn || !jsonBtn) return;

        var activeClass = 'bg-hot-red-500 text-white';
        var inactiveClass = 'bg-gray-200 dark:bg-gray-700 text-gray-700 dark:text-gray-300 hover:bg-gray-300 dark:hover:bg-gray-600';

        hotBtn.className = 'px-2 py-1 text-xs rounded transition-colors ' + (activeFormat === 'hot' ? activeClass : inactiveClass);
        jsonBtn.className = 'px-2 py-1 text-xs rounded transition-colors ' + (activeFormat === 'json' ? activeClass : inactiveClass);
    }

    /**
     * Format value for modal based on format type
     */
    function formatValueForModal(value, format) {
        if (format === 'json') {
            return formatAsJson(value, 0);
        } else {
            return formatAsHotLiteral(value, 0);
        }
    }

    /**
     * Close content modal
     */
    function closeContentModal(event) {
        if (!event || event.target === event.currentTarget || event.target.closest('button[onclick*="closeContentModal"]')) {
            var modal = document.getElementById('content-modal');
            if (modal) {
                modal.classList.add('hidden');
                document.body.style.overflow = '';
            }
        }
    }

    /**
     * Copy modal content to clipboard
     */
    function copyModalContent() {
        var modalBody = document.getElementById('content-modal-body');
        if (!modalBody) return;

        var text = modalBody.textContent || '';
        navigator.clipboard.writeText(text).then(function() {
            // Show "Copied!" feedback
            var copyText = document.getElementById('content-modal-copy-text');
            if (copyText) {
                var originalText = copyText.textContent;
                copyText.textContent = 'Copied!';
                setTimeout(function() {
                    copyText.textContent = originalText;
                }, 1500);
            }
        }).catch(function(err) {
            console.error('Failed to copy:', err);
        });
    }

    /**
     * Select all content in modal (for Ctrl+A)
     */
    function selectModalContent() {
        var modalBody = document.getElementById('content-modal-body');
        if (!modalBody) return;

        var selection = window.getSelection();
        var range = document.createRange();
        range.selectNodeContents(modalBody);
        selection.removeAllRanges();
        selection.addRange(range);
    }

    /**
     * Check if content modal is open
     */
    function isContentModalOpen() {
        var modal = document.getElementById('content-modal');
        return modal && !modal.classList.contains('hidden');
    }

    // ============================================
    // Source Browser Modal
    // ============================================

    var sourceBrowserState = {
        buildId: null,
        files: [],
        expandedFolders: {},
        searchCaseSensitive: false,
        searchRegex: false,
        searchTimer: null,
        searchRequestId: 0,
        currentPath: null,
        currentContent: '',
        targetLine: null
    };

    function openSourceBrowserFromElement(element) {
        if (!element) return;
        openSourceBrowser({
            buildId: element.getAttribute('data-source-build-id'),
            file: element.getAttribute('data-source-file'),
            line: element.getAttribute('data-source-line'),
            title: element.getAttribute('data-source-title')
        });
    }

    function openSourceBrowser(options) {
        options = options || {};
        if (!options.buildId) return;

        var modal = document.getElementById('source-browser-modal');
        if (!modal) return;

        sourceBrowserState.buildId = options.buildId;
        sourceBrowserState.expandedFolders = {};
        sourceBrowserState.searchCaseSensitive = false;
        sourceBrowserState.searchRegex = false;
        sourceBrowserState.searchRequestId += 1;
        sourceBrowserState.currentPath = null;
        sourceBrowserState.currentContent = '';
        sourceBrowserState.targetLine = parsePositiveInt(options.line);
        resetSourceBrowserSearchControls();

        setSourceBrowserTitle(options.title || 'Source', '');
        setSourceBrowserStatus('Loading source files...');
        clearSourceBrowserCode();

        modal.classList.remove('hidden');
        document.body.style.overflow = 'hidden';

        fetch('/source/' + encodeURIComponent(options.buildId) + '/tree')
            .then(function(response) {
                if (!response.ok) throw new Error('Failed to load source tree');
                return response.json();
            })
            .then(function(tree) {
                sourceBrowserState.files = tree.files || [];
                expandAllSourceBrowserFolders(sourceBrowserState.files);
                renderSourceBrowserTree(sourceBrowserState.files);
                if (options.file) {
                    loadSourceBrowserFile(options.file, sourceBrowserState.targetLine);
                } else if (sourceBrowserState.files.length > 0) {
                    loadSourceBrowserFile(sourceBrowserState.files[0].path, null);
                } else {
                    setSourceBrowserStatus('No source files found for this build.');
                }
            })
            .catch(function(error) {
                console.error(error);
                setSourceBrowserStatus(error.message || 'Failed to load source files.');
            });
    }

    function closeSourceBrowser(event) {
        if (!event || event.target === event.currentTarget || event.target.closest('button[onclick*="closeSourceBrowser"]')) {
            var modal = document.getElementById('source-browser-modal');
            if (modal) {
                modal.classList.add('hidden');
                document.body.style.overflow = '';
            }
        }
    }

    function isSourceBrowserOpen() {
        var modal = document.getElementById('source-browser-modal');
        return modal && !modal.classList.contains('hidden');
    }

    function loadSourceBrowserFile(path, line) {
        if (!sourceBrowserState.buildId || !path) return;

        var params = new URLSearchParams();
        params.set('path', path);
        if (line) params.set('line', String(line));

        setSourceBrowserStatus('Loading ' + path + '...');

        fetch('/source/' + encodeURIComponent(sourceBrowserState.buildId) + '/file?' + params.toString())
            .then(function(response) {
                if (!response.ok) throw new Error('Failed to load source file');
                return response.json();
            })
            .then(function(file) {
                sourceBrowserState.currentPath = file.path;
                sourceBrowserState.currentContent = file.content || '';
                sourceBrowserState.targetLine = parsePositiveInt(file.line);
                setSourceBrowserTitle(file.display_path || file.path, file.build_type || '');
                setSourceBrowserStatus((file.display_path || file.path) + (sourceBrowserState.targetLine ? ':' + sourceBrowserState.targetLine : ''));
                renderSourceBrowserCode(sourceBrowserState.currentContent, sourceBrowserState.targetLine, file.language || 'hot');
                updateSourceBrowserActiveFile(file.path);
            })
            .catch(function(error) {
                console.error(error);
                setSourceBrowserStatus(error.message || 'Failed to load source file.');
            });
    }

    function renderSourceBrowserTree(files) {
        var tree = document.getElementById('source-browser-tree');
        if (!tree) return;

        var input = document.getElementById('source-browser-filter');
        var query = input ? input.value.toLowerCase() : '';
        var filteredFiles = query
            ? files.filter(function(file) {
                return (file.path || '').toLowerCase().indexOf(query) !== -1;
            })
            : files;

        tree.innerHTML = '';
        if (!files || files.length === 0) {
            tree.textContent = 'No source files found.';
            return;
        }

        if (filteredFiles.length === 0) {
            tree.textContent = 'No files match the filter.';
            return;
        }

        var root = buildSourceBrowserFileTree(filteredFiles);
        renderSourceBrowserTreeNodes(tree, root.children, 0, query);
        updateSourceBrowserActiveFile(sourceBrowserState.currentPath, true);
    }

    function buildSourceBrowserFileTree(files) {
        var root = { name: '', path: '', type: 'folder', children: {}, file: null };

        files.forEach(function(file) {
            if (!file || !file.path) return;
            var parts = file.path.split('/').filter(Boolean);
            var node = root;
            var pathParts = [];

            parts.forEach(function(part, index) {
                pathParts.push(part);
                var path = pathParts.join('/');
                var isFile = index === parts.length - 1;
                if (!node.children[part]) {
                    node.children[part] = {
                        name: part,
                        path: path,
                        type: isFile ? 'file' : 'folder',
                        children: {},
                        file: isFile ? file : null
                    };
                }
                if (isFile) {
                    node.children[part].type = 'file';
                    node.children[part].file = file;
                } else {
                    node = node.children[part];
                }
            });
        });

        return root;
    }

    function renderSourceBrowserTreeNodes(container, children, depth, forceExpanded) {
        Object.keys(children)
            .sort(function(a, b) {
                var nodeA = children[a];
                var nodeB = children[b];
                if (nodeA.type !== nodeB.type) return nodeA.type === 'folder' ? -1 : 1;
                return nodeA.name.localeCompare(nodeB.name);
            })
            .forEach(function(name) {
                var node = children[name];
                if (node.type === 'folder') {
                    renderSourceBrowserFolder(container, node, depth, forceExpanded);
                } else {
                    renderSourceBrowserFile(container, node.file, depth);
                }
            });
    }

    function renderSourceBrowserFolder(container, node, depth, forceExpanded) {
        var isExpanded = forceExpanded || sourceBrowserState.expandedFolders[node.path] === true;

        var button = document.createElement('button');
        button.type = 'button';
        button.setAttribute('data-source-folder', node.path);
        button.className = 'source-browser-folder flex items-center w-full text-left pr-2 py-1 rounded text-xs font-mono text-gray-600 dark:text-gray-400 hover:bg-gray-100 dark:hover:bg-gray-800 truncate';
        button.style.paddingLeft = String(4 + depth * 16) + 'px';
        button.title = node.path;
        button.onclick = function() {
            sourceBrowserState.expandedFolders[node.path] = !isExpanded;
            renderSourceBrowserTree(sourceBrowserState.files);
        };

        var caret = document.createElement('span');
        caret.className = 'inline-flex w-4 h-4 mr-0.5 flex-shrink-0 items-center justify-center text-gray-400 dark:text-gray-500';
        caret.appendChild(sourceBrowserChevronIcon(isExpanded));

        var label = document.createElement('span');
        label.className = 'truncate';
        label.textContent = node.name;

        button.appendChild(caret);
        button.appendChild(label);
        container.appendChild(button);

        if (isExpanded) {
            renderSourceBrowserTreeNodes(container, node.children, depth + 1, forceExpanded);
        }
    }

    function sourceBrowserChevronIcon(isExpanded) {
        var svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
        svg.setAttribute('viewBox', '0 0 16 16');
        svg.setAttribute('aria-hidden', 'true');
        svg.setAttribute('width', '14');
        svg.setAttribute('height', '14');
        svg.style.transform = isExpanded ? 'rotate(90deg)' : 'rotate(0deg)';
        svg.style.transition = 'transform 120ms ease';

        var path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
        path.setAttribute('d', 'M6 4l4 4-4 4');
        path.setAttribute('fill', 'none');
        path.setAttribute('stroke', 'currentColor');
        path.setAttribute('stroke-width', '1.6');
        path.setAttribute('stroke-linecap', 'round');
        path.setAttribute('stroke-linejoin', 'round');
        svg.appendChild(path);

        return svg;
    }

    function renderSourceBrowserFile(container, file, depth) {
        if (!file) return;

        var name = file.path.split('/').filter(Boolean).pop() || file.path;
        var button = document.createElement('button');
        button.type = 'button';
        button.setAttribute('data-source-path', file.path);
        button.className = sourceBrowserFileClass(false);
        button.style.paddingLeft = String(4 + depth * 16 + 18) + 'px';
        button.title = file.path;
        button.appendChild(sourceBrowserFileIcon(file.path));

        var label = document.createElement('span');
        label.className = 'truncate';
        label.textContent = name;
        button.appendChild(label);

        button.onclick = function() {
            loadSourceBrowserFile(file.path, null);
        };
        container.appendChild(button);
    }

    function sourceBrowserFileIcon(path) {
        var meta = sourceBrowserFileIconMeta(path);
        var icon = document.createElement('span');
        icon.className = meta.className;
        icon.title = meta.title;
        if (meta.color) icon.style.color = meta.color;

        if (meta.hot) {
            icon.innerHTML = '<svg viewBox="0 0 1800 1800" aria-hidden="true" class="w-3.5 h-3.5"><path fill="currentColor" fill-rule="evenodd" d="M 772.00,100.21 C 772.00,100.21 844.00,100.21 844.00,100.21 844.00,100.21 879.00,102.83 879.00,102.83 879.00,102.83 896.00,104.17 896.00,104.17 931.29,107.70 969.58,115.02 1004.00,123.63 1136.51,156.75 1258.01,222.93 1358.00,316.09 1358.00,316.09 1390.09,349.00 1390.09,349.00 1486.31,452.28 1551.12,567.98 1585.37,705.00 1595.81,746.74 1602.08,787.26 1606.17,830.00 1606.17,830.00 1608.09,853.00 1608.09,853.00 1608.09,853.00 1609.00,863.00 1609.00,863.00 1609.00,863.00 1609.00,935.00 1609.00,935.00 1609.00,935.00 1605.17,981.00 1605.17,981.00 1599.92,1035.20 1589.15,1087.97 1572.97,1140.00 1537.51,1254.09 1474.28,1361.66 1392.91,1449.00 1392.91,1449.00 1360.00,1481.09 1360.00,1481.09 1278.47,1557.05 1186.22,1615.87 1081.00,1653.31 1032.16,1670.69 972.42,1686.15 921.00,1692.72 921.00,1692.72 896.00,1695.17 896.00,1695.17 896.00,1695.17 844.00,1700.00 844.00,1700.00 844.00,1700.00 772.00,1700.00 772.00,1700.00 772.00,1700.00 762.00,1699.09 762.00,1699.09 762.00,1699.09 723.00,1695.17 723.00,1695.17 723.00,1695.17 697.00,1692.71 697.00,1692.71 639.95,1685.60 563.47,1664.60 510.00,1643.40 510.00,1643.40 460.00,1622.22 460.00,1622.22 431.89,1608.94 381.79,1578.84 358.00,1559.54 347.83,1551.29 338.25,1542.25 329.00,1533.00 295.17,1499.17 265.55,1458.30 243.22,1416.00 243.22,1416.00 232.58,1395.00 232.58,1395.00 208.61,1340.04 193.59,1291.40 190.96,1231.00 190.96,1231.00 190.00,1220.00 190.00,1220.00 190.00,1220.00 190.00,1204.00 190.00,1204.00 190.03,1182.32 195.83,1157.52 202.67,1137.00 215.88,1097.37 234.65,1063.15 256.95,1028.00 302.78,955.73 371.87,873.26 426.00,805.00 455.49,767.82 484.02,729.93 509.69,690.00 546.13,633.33 571.80,580.71 571.00,512.00 571.00,512.00 570.09,502.00 570.09,502.00 568.56,481.13 564.23,460.96 558.03,441.00 540.02,383.08 507.88,333.93 473.00,285.00 473.00,285.00 430.42,225.00 430.42,225.00 426.85,218.85 419.65,207.21 423.31,200.04 425.62,195.54 440.89,188.37 446.00,186.03 446.00,186.03 470.00,174.31 470.00,174.31 470.00,174.31 503.00,159.99 503.00,159.99 566.84,133.65 628.75,117.32 697.00,107.27 697.00,107.27 762.00,101.00 762.00,101.00 766.45,100.98 767.47,101.07 772.00,100.21 Z M 1031.00,1523.00 C 1038.83,1520.89 1057.18,1512.15 1065.00,1508.25 1089.30,1496.10 1112.25,1482.18 1134.00,1465.87 1162.55,1444.46 1190.32,1418.30 1212.11,1390.00 1246.14,1345.80 1270.57,1296.13 1284.11,1242.00 1304.93,1158.71 1295.69,1065.92 1262.58,987.00 1203.32,845.78 1077.90,746.14 932.00,705.58 897.21,695.90 861.08,689.53 825.00,687.96 825.00,687.96 814.00,687.00 814.00,687.00 814.00,687.00 774.00,687.00 774.00,687.00 774.00,687.00 762.00,687.91 762.00,687.91 762.00,687.91 737.00,689.93 737.00,689.93 737.00,689.93 719.00,692.13 719.00,692.13 716.80,692.48 713.59,692.88 712.69,695.30 711.58,698.31 715.19,705.27 716.64,708.00 716.64,708.00 736.42,740.00 736.42,740.00 755.61,770.80 780.56,814.88 781.00,852.00 781.15,864.86 780.29,875.73 775.91,888.00 766.85,913.40 746.30,934.11 727.00,952.09 705.02,972.56 681.75,991.28 658.00,1009.65 612.84,1044.59 566.46,1078.54 526.00,1119.00 503.13,1141.87 479.35,1169.20 468.05,1200.00 463.19,1213.25 459.17,1232.90 458.92,1247.00 458.92,1247.00 458.92,1256.00 458.92,1256.00 458.50,1259.47 457.92,1261.22 458.04,1265.00 458.04,1265.00 459.00,1277.00 459.00,1277.00 459.08,1292.39 462.38,1309.09 466.13,1324.00 471.84,1346.76 481.75,1370.87 493.80,1391.00 507.57,1414.00 521.94,1433.03 541.00,1452.00 548.17,1459.14 564.92,1473.15 574.00,1477.00 570.80,1462.97 566.17,1449.64 566.00,1435.00 566.00,1435.00 566.00,1420.00 566.00,1420.00 566.05,1390.46 580.74,1348.65 598.16,1325.00 611.50,1306.87 631.69,1290.34 650.00,1277.29 676.37,1258.50 704.62,1241.89 733.00,1226.30 761.80,1210.49 800.15,1189.61 826.00,1170.37 845.98,1155.49 860.58,1139.32 859.99,1113.00 859.69,1099.92 854.86,1084.95 849.72,1073.00 845.86,1064.03 840.22,1054.94 840.00,1045.00 840.00,1045.00 868.00,1043.00 868.00,1043.00 891.14,1042.96 914.20,1044.31 937.00,1048.61 1007.35,1061.88 1073.60,1098.89 1114.26,1159.00 1128.03,1179.37 1137.20,1199.66 1144.66,1223.00 1149.02,1236.65 1153.98,1263.79 1154.00,1278.00 1154.00,1278.00 1154.00,1303.00 1154.00,1303.00 1153.98,1318.28 1148.98,1344.12 1144.71,1359.00 1131.78,1404.03 1108.24,1445.27 1076.83,1480.00 1068.79,1488.89 1060.00,1497.80 1051.00,1505.72 1051.00,1505.72 1031.00,1523.00 1031.00,1523.00 Z"/></svg>';
        } else {
            icon.textContent = meta.label;
        }

        return icon;
    }

    function sourceBrowserFileIconMeta(path) {
        var name = (path || '').split('/').filter(Boolean).pop() || '';
        var lower = name.toLowerCase();

        if (lower.endsWith('.hot')) {
            return {
                hot: true,
                title: 'Hot',
                color: '#f98704',
                className: 'inline-flex w-4 h-4 mr-1 flex-shrink-0 items-center justify-center'
            };
        }

        var label = 'TXT';
        var color = '#888888';
        if (lower.endsWith('.skill.md')) {
            label = 'SK';
            color = '#a78bfa';
        } else if (lower.endsWith('.md')) {
            label = 'MD';
            color = '#60a5fa';
        } else if (lower.endsWith('.py')) {
            label = 'PY';
            color = '#facc15';
        } else if (lower.endsWith('.ts') || lower.endsWith('.tsx')) {
            label = 'TS';
            color = '#60a5fa';
        } else if (lower.endsWith('.js') || lower.endsWith('.jsx')) {
            label = 'JS';
            color = '#facc15';
        } else if (lower.endsWith('.sh') || lower.endsWith('.bash') || lower.endsWith('.ps1')) {
            label = '$';
            color = '#4ade80';
        } else if (lower.endsWith('.json') || lower.endsWith('.yaml') || lower.endsWith('.yml') || lower.endsWith('.toml') || lower.endsWith('.env') || lower.endsWith('.ini')) {
            label = '{}';
            color = '#22d3ee';
        } else if (lower.endsWith('.html') || lower.endsWith('.css') || lower.endsWith('.svg') || lower.endsWith('.xml')) {
            label = '<>';
            color = '#fb923c';
        } else if (lower === 'dockerfile' || lower.endsWith('.dockerfile')) {
            label = 'DK';
            color = '#60a5fa';
        }

        return {
            hot: false,
            label: label,
            title: label,
            color: color,
            className: 'inline-flex w-4 h-4 mr-1 flex-shrink-0 items-center justify-center rounded-sm text-[8px] font-semibold leading-none'
        };
    }

    function filterSourceBrowserFiles() {
        renderSourceBrowserTree(sourceBrowserState.files);
    }

    function resetSourceBrowserSearchControls() {
        var search = document.getElementById('source-browser-search');
        var message = document.getElementById('source-browser-search-message');
        var results = document.getElementById('source-browser-search-results');
        var tree = document.getElementById('source-browser-tree');
        if (search) search.value = '';
        if (message) {
            message.classList.add('hidden');
            message.textContent = '';
        }
        if (results) {
            results.classList.add('hidden');
            results.innerHTML = '';
        }
        if (tree) tree.classList.remove('hidden');
        updateSourceBrowserSearchToggleButtons();
    }

    function toggleSourceBrowserCaseSensitive() {
        sourceBrowserState.searchCaseSensitive = !sourceBrowserState.searchCaseSensitive;
        updateSourceBrowserSearchToggleButtons();
        searchSourceBrowserFiles();
    }

    function toggleSourceBrowserRegex() {
        sourceBrowserState.searchRegex = !sourceBrowserState.searchRegex;
        updateSourceBrowserSearchToggleButtons();
        searchSourceBrowserFiles();
    }

    function updateSourceBrowserSearchToggleButtons() {
        updateSourceBrowserSearchToggleButton('source-browser-case-toggle', sourceBrowserState.searchCaseSensitive);
        updateSourceBrowserSearchToggleButton('source-browser-regex-toggle', sourceBrowserState.searchRegex);
    }

    function updateSourceBrowserSearchToggleButton(id, active) {
        var button = document.getElementById(id);
        if (!button) return;
        button.className = active
            ? sourceBrowserSearchToggleClass(id, true)
            : sourceBrowserSearchToggleClass(id, false);
    }

    function sourceBrowserSearchToggleClass(id, active) {
        var isRegex = id === 'source-browser-regex-toggle';
        var textSize = isRegex ? 'text-xs' : 'text-[10px]';
        var edgeMargin = isRegex ? 'mr-1' : 'mr-0.5';
        return active
            ? edgeMargin + ' inline-flex h-6 min-w-6 items-center justify-center rounded px-1.5 ' + textSize + ' leading-none bg-gray-200 dark:bg-gray-700 text-gray-900 dark:text-gray-100'
            : (id === 'source-browser-regex-toggle'
                ? edgeMargin + ' inline-flex h-6 min-w-6 items-center justify-center rounded px-1.5 text-xs leading-none text-gray-500 dark:text-gray-400 hover:text-gray-900 dark:hover:text-gray-100'
                : edgeMargin + ' inline-flex h-6 min-w-6 items-center justify-center rounded px-1.5 text-[10px] leading-none text-gray-500 dark:text-gray-400 hover:text-gray-900 dark:hover:text-gray-100');
    }

    function searchSourceBrowserFiles() {
        if (sourceBrowserState.searchTimer) clearTimeout(sourceBrowserState.searchTimer);
        sourceBrowserState.searchTimer = setTimeout(runSourceBrowserSearch, 250);
    }

    function runSourceBrowserSearch() {
        var input = document.getElementById('source-browser-search');
        var tree = document.getElementById('source-browser-tree');
        var results = document.getElementById('source-browser-search-results');
        var query = input ? input.value.trim() : '';
        var requestId = sourceBrowserState.searchRequestId + 1;
        sourceBrowserState.searchRequestId = requestId;

        if (!tree || !results) return;
        if (!query) {
            results.classList.add('hidden');
            results.innerHTML = '';
            tree.classList.remove('hidden');
            setSourceBrowserSearchMessage('', false);
            return;
        }

        tree.classList.add('hidden');
        results.classList.remove('hidden');
        results.innerHTML = '<div class="px-2 py-1 text-xs text-gray-500 dark:text-gray-400">Searching...</div>';
        setSourceBrowserSearchMessage('', false);

        var params = new URLSearchParams();
        params.set('q', query);
        params.set('case_sensitive', sourceBrowserState.searchCaseSensitive ? 'true' : 'false');
        params.set('regex', sourceBrowserState.searchRegex ? 'true' : 'false');

        fetch('/source/' + encodeURIComponent(sourceBrowserState.buildId) + '/search?' + params.toString())
            .then(function(response) {
                return response.json().then(function(body) {
                    if (!response.ok) {
                        throw new Error(body && body.error ? body.error : 'Search failed');
                    }
                    return body;
                });
            })
            .then(function(body) {
                if (requestId !== sourceBrowserState.searchRequestId) return;
                renderSourceBrowserSearchResults(body.results || [], body.truncated);
            })
            .catch(function(error) {
                if (requestId !== sourceBrowserState.searchRequestId) return;
                results.innerHTML = '';
                setSourceBrowserSearchMessage(error.message || 'Search failed.', true);
            });
    }

    function renderSourceBrowserSearchResults(matches, truncated) {
        var results = document.getElementById('source-browser-search-results');
        if (!results) return;

        results.innerHTML = '';
        if (!matches || matches.length === 0) {
            results.innerHTML = '<div class="px-2 py-1 text-xs text-gray-500 dark:text-gray-400">No results found.</div>';
            setSourceBrowserSearchMessage('', false);
            return;
        }

        var byPath = {};
        var paths = [];
        matches.forEach(function(match) {
            if (!byPath[match.path]) {
                byPath[match.path] = [];
                paths.push(match.path);
            }
            byPath[match.path].push(match);
        });

        paths.forEach(function(path) {
            var group = document.createElement('div');
            group.className = 'mb-2';

            var heading = document.createElement('div');
            heading.className = 'px-2 py-1 text-xs font-mono font-semibold text-gray-700 dark:text-gray-300 truncate';
            heading.title = path;
            heading.textContent = path;
            group.appendChild(heading);

            byPath[path].forEach(function(match) {
                var button = document.createElement('button');
                button.type = 'button';
                button.className = 'block w-full text-left px-2 py-1 rounded hover:bg-gray-100 dark:hover:bg-gray-800 text-xs';
                button.title = path + ':' + match.line;
                button.onclick = function() {
                    loadSourceBrowserFile(match.path, match.line);
                };

                var line = document.createElement('div');
                line.className = 'flex gap-2 min-w-0 font-mono';

                var number = document.createElement('span');
                number.className = 'text-gray-400 dark:text-gray-600 flex-shrink-0';
                number.textContent = String(match.line);

                var text = document.createElement('span');
                text.className = 'text-gray-700 dark:text-gray-300 truncate';
                text.textContent = (match.line_text || '').trim() || ' ';

                line.appendChild(number);
                line.appendChild(text);
                button.appendChild(line);
                group.appendChild(button);
            });

            results.appendChild(group);
        });

        setSourceBrowserSearchMessage(
            matches.length + ' result' + (matches.length === 1 ? '' : 's') + (truncated ? ' (limited)' : ''),
            false
        );
    }

    function setSourceBrowserSearchMessage(message, isError) {
        var el = document.getElementById('source-browser-search-message');
        if (!el) return;
        if (!message) {
            el.classList.add('hidden');
            el.textContent = '';
            return;
        }
        el.className = isError
            ? 'mt-1 text-xs text-red-600 dark:text-red-400'
            : 'mt-1 text-xs text-gray-500 dark:text-gray-400';
        el.textContent = message;
    }

    function expandAllSourceBrowserFolders(files) {
        files.forEach(function(file) {
            expandSourceBrowserPath(file.path);
        });
    }

    function expandSourceBrowserPath(path) {
        if (!path) return;
        var parts = path.split('/').filter(Boolean);
        var current = [];
        for (var i = 0; i < parts.length - 1; i += 1) {
            current.push(parts[i]);
            sourceBrowserState.expandedFolders[current.join('/')] = true;
        }
    }

    function sourceBrowserFileClass(active) {
        return active
            ? 'source-browser-file flex items-center w-full text-left px-2 py-1 rounded text-xs font-mono bg-hot-red-50 dark:bg-hot-red-900/30 text-hot-red-700 dark:text-hot-red-200 truncate'
            : 'source-browser-file flex items-center w-full text-left px-2 py-1 rounded text-xs font-mono text-gray-700 dark:text-gray-300 hover:bg-gray-100 dark:hover:bg-gray-800 truncate';
    }

    function updateSourceBrowserActiveFile(path, skipRender) {
        if (path && !skipRender) {
            expandSourceBrowserPath(path);
            renderSourceBrowserTree(sourceBrowserState.files);
            return;
        }

        document.querySelectorAll('.source-browser-file').forEach(function(button) {
            var active = button.getAttribute('data-source-path') === path;
            button.className = sourceBrowserFileClass(active);
        });
    }

    function renderSourceBrowserCode(content, targetLine, language) {
        var empty = document.getElementById('source-browser-empty');
        var table = document.getElementById('source-browser-code-table');
        var body = document.getElementById('source-browser-code-body');
        if (!empty || !table || !body) return;

        body.innerHTML = '';
        empty.classList.add('hidden');
        table.classList.remove('hidden');

        var lines = content.split('\n');
        var codeClass = sourceBrowserCodeClass(language);
        var highlightedLines = sourceBrowserHighlightedLines(content, language);

        lines.forEach(function(line, index) {
            var lineNumber = index + 1;
            var row = document.createElement('tr');
            row.id = 'source-line-' + lineNumber;
            row.className = lineNumber === targetLine ? 'bg-hot-red-50 dark:bg-hot-red-900/20' : '';

            var gutter = document.createElement('td');
            gutter.className = 'select-none text-right text-gray-400 dark:text-gray-600 pr-4 pl-3 py-0 align-top border-r border-gray-200 dark:border-gray-800 w-1';
            gutter.textContent = String(lineNumber);

            var codeCell = document.createElement('td');
            codeCell.className = 'px-4 py-0 align-top whitespace-pre';

            var code = document.createElement('code');
            code.className = codeClass;
            if (highlightedLines) {
                code.innerHTML = highlightedLines[index] || ' ';
            } else {
                code.textContent = line || ' ';
            }
            codeCell.appendChild(code);

            row.appendChild(gutter);
            row.appendChild(codeCell);
            body.appendChild(row);
        });

        if (targetLine) {
            setTimeout(function() {
                var target = document.getElementById('source-line-' + targetLine);
                if (target) {
                    target.scrollIntoView({ block: 'center' });
                }
            }, 0);
        }
    }

    function sourceBrowserCodeClass(language) {
        if (!language || language === 'plain') return 'language-none';
        if (typeof Prism === 'undefined' || !Prism.languages || !Prism.languages[language]) {
            return 'language-none';
        }
        return 'language-' + language;
    }

    function sourceBrowserHighlightedLines(content, language) {
        if (!language || language === 'plain' || typeof Prism === 'undefined' || !Prism.languages || !Prism.languages[language]) {
            return null;
        }

        var lines = [''];
        var tokens = Prism.tokenize(content, Prism.languages[language]);
        appendSourceBrowserTokenLines(tokens, language, lines);
        return lines;
    }

    function appendSourceBrowserTokenLines(tokenOrTokens, language, lines) {
        if (Array.isArray(tokenOrTokens)) {
            tokenOrTokens.forEach(function(token) {
                appendSourceBrowserTokenLines(token, language, lines);
            });
            return;
        }

        if (typeof tokenOrTokens === 'string') {
            appendSourceBrowserTextLines(tokenOrTokens, lines);
            return;
        }

        var innerLines = [''];
        appendSourceBrowserTokenLines(tokenOrTokens.content, language, innerLines);
        innerLines.forEach(function(lineHtml, index) {
            if (index > 0) lines.push('');
            lines[lines.length - 1] += wrapSourceBrowserPrismToken(tokenOrTokens, lineHtml, language);
        });
    }

    function appendSourceBrowserTextLines(text, lines) {
        var parts = String(text).split('\n');
        parts.forEach(function(part, index) {
            if (index > 0) lines.push('');
            lines[lines.length - 1] += escapeSourceBrowserHtml(part);
        });
    }

    function wrapSourceBrowserPrismToken(token, content, language) {
        var classes = ['token', token.type];
        if (token.alias) {
            classes = classes.concat(Array.isArray(token.alias) ? token.alias : [token.alias]);
        }

        var env = {
            type: token.type,
            content: content,
            tag: 'span',
            classes: classes,
            attributes: {},
            language: language
        };
        Prism.hooks.run('wrap', env);

        var attributes = '';
        Object.keys(env.attributes).forEach(function(name) {
            attributes += ' ' + name + '="' + String(env.attributes[name]).replace(/"/g, '&quot;') + '"';
        });

        return '<' + env.tag + ' class="' + env.classes.join(' ') + '"' + attributes + '>' + env.content + '</' + env.tag + '>';
    }

    function escapeSourceBrowserHtml(text) {
        return String(text)
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;');
    }

    function clearSourceBrowserCode() {
        var empty = document.getElementById('source-browser-empty');
        var table = document.getElementById('source-browser-code-table');
        var body = document.getElementById('source-browser-code-body');
        if (empty) empty.classList.remove('hidden');
        if (table) table.classList.add('hidden');
        if (body) body.innerHTML = '';
    }

    function setSourceBrowserTitle(title, subtitle) {
        var titleEl = document.getElementById('source-browser-title');
        var subtitleEl = document.getElementById('source-browser-subtitle');
        if (titleEl) titleEl.textContent = title || 'Source';
        if (subtitleEl) subtitleEl.textContent = subtitle || '';
    }

    function setSourceBrowserStatus(status) {
        var statusEl = document.getElementById('source-browser-status');
        if (statusEl) statusEl.textContent = status || '';
    }

    function copySourceBrowserContent() {
        if (!sourceBrowserState.currentContent) return;
        navigator.clipboard.writeText(sourceBrowserState.currentContent).then(function() {
            var button = document.getElementById('source-browser-copy-btn');
            if (!button) return;
            var originalText = button.textContent;
            button.textContent = 'Copied!';
            setTimeout(function() {
                button.textContent = originalText;
            }, 1500);
        }).catch(function(err) {
            console.error('Failed to copy source:', err);
        });
    }

    function parsePositiveInt(value) {
        var parsed = parseInt(value, 10);
        return Number.isFinite(parsed) && parsed > 0 ? parsed : null;
    }

    /**
     * Open modal from element content or data attribute
     */
    function openContentModalFromElement(element, title) {
        var content = element.getAttribute('data-modal-content');
        if (!content) {
            content = element.textContent;
        }
        if (content) {
            var decodedContent = decodeHtmlEntities(content);
            try {
                var parsed = JSON.parse(decodedContent);
                openContentModal(title, decodedContent, parsed);
            } catch (e) {
                openContentModal(title, decodedContent);
            }
        }
    }

    /**
     * Open modal from script tag
     */
    function openContentModalFromScript(scriptId, title) {
        var scriptEl = document.getElementById(scriptId);
        if (scriptEl) {
            var content = scriptEl.textContent;
            var decodedContent = decodeHtmlEntities(content);
            try {
                var parsed = JSON.parse(decodedContent);
                openContentModal(title, decodedContent, parsed);
            } catch (e) {
                openContentModal(title, decodedContent);
            }
        }
    }

    /**
     * Open a plain-text modal without JSON/Hot format detection.
     */
    function openPlainTextModal(title, content) {
        openContentModal(title, decodeHtmlEntities(content || ''));
    }

    /**
     * Open a plain-text modal from a script tag.
     */
    function openPlainTextModalFromScript(scriptId, title) {
        var scriptEl = document.getElementById(scriptId);
        if (scriptEl) {
            openPlainTextModal(title, scriptEl.textContent);
        }
    }

    /**
     * Open modal with raw data
     */
    function openContentModalWithData(title, rawData) {
        openContentModal(title, null, rawData);
    }

    /**
     * Open event data modal from element attributes
     */
    function openEventDataFromElement(element) {
        if (element) {
            var hotContent = element.getAttribute('data-event-data-hot');
            var jsonContent = element.getAttribute('data-event-data-json');
            if (hotContent && jsonContent) {
                openContentModalWithFormats('Event Data', hotContent, jsonContent);
            } else if (hotContent) {
                openContentModal('Event Data', hotContent);
            }
        }
    }

    /**
     * Decode HTML entities
     */
    function decodeHtmlEntities(text) {
        var textarea = document.createElement('textarea');
        textarea.innerHTML = text;
        return textarea.value;
    }

    // ============================================
    // UUID Utilities
    // ============================================

    /**
     * Copy text to clipboard
     */
    function copyToClipboard(text) {
        navigator.clipboard.writeText(text).then(function() {
            // Success
        }).catch(function(err) {
            console.error('Failed to copy:', err);
        });
    }

    /**
     * Truncate UUID to last 12 characters
     */
    function truncateUuid(uuid) {
        var uuidNoHyphens = uuid.replace(/-/g, '');
        if (uuidNoHyphens.length >= 12) {
            return uuidNoHyphens.slice(-12);
        }
        return uuidNoHyphens;
    }

    /**
     * Initialize UUID displays and copy functionality
     */
    function initUuidDisplay() {
        // Truncate all UUID displays
        document.querySelectorAll('.uuid-display').forEach(function(span) {
            var fullUuid = span.getAttribute('data-full-uuid');
            if (fullUuid) {
                span.textContent = truncateUuid(fullUuid);
            }
        });
    }

    /**
     * Handle UUID copy button clicks (via event delegation)
     */
    function handleUuidCopyClick(e) {
        if (e.target.closest('.uuid-copy-btn')) {
            e.preventDefault();
            var button = e.target.closest('.uuid-copy-btn');
            var uuid = button.getAttribute('data-uuid');
            if (uuid) {
                copyToClipboard(uuid);

                // Visual feedback
                var originalTitle = button.title;
                button.title = 'Copied!';
                button.classList.add('text-green-600', 'dark:text-green-400');

                setTimeout(function() {
                    button.title = originalTitle;
                    button.classList.remove('text-green-600', 'dark:text-green-400');
                }, 1000);
            }
        }
    }

    // ============================================
    // Sidebar Management
    // ============================================

    var expandSidebarFunc = null;

    /**
     * Handle sidebar dropdown clicks (called from Alpine)
     */
    function handleSidebarDropdownClick(dropdownType, event) {
        var sidebar = document.getElementById('sidebar');
        if (!sidebar) return true;

        var isMobile = window.innerWidth < 768;

        if (isMobile) {
            if (!sidebar.classList.contains('sidebar-mobile-open')) {
                sidebar.classList.add('sidebar-mobile-open');
                var mobileOverlay = document.getElementById('sidebar-mobile-overlay');
                if (mobileOverlay) {
                    mobileOverlay.classList.add('active');
                }
                document.body.style.overflow = 'hidden';
                return false;
            }
            return true;
        } else {
            var isCollapsed = sidebar.classList.contains('collapsed');
            if (isCollapsed) {
                if (expandSidebarFunc) {
                    expandSidebarFunc();
                } else {
                    sidebar.style.setProperty('width', '240px', 'important');
                    sidebar.classList.remove('collapsed');
                    localStorage.setItem('sidebar-collapsed', 'false');
                }
                return false;
            }
            return true;
        }
    }

    /**
     * Initialize sidebar functionality
     */
    function initSidebar() {
        var sidebarToggleBtn = document.getElementById('sidebar-toggle-button');
        var sidebar = document.getElementById('sidebar');
        var headerLeftSection = document.getElementById('header-left-section');
        var sidebarLogo = document.querySelector('.sidebar-logo');
        var mainLogo = document.querySelector('.main-logo');
        var mobileOverlay = document.getElementById('sidebar-mobile-overlay');

        if (!sidebar || !sidebarToggleBtn) return;

        function isMobile() {
            return window.innerWidth < 768;
        }

        function isTablet() {
            return window.innerWidth >= 768 && window.innerWidth < 1024;
        }

        function setCollapsedState() {
            if (isMobile()) return;

            sidebar.style.setProperty('width', '64px', 'important');
            sidebar.classList.add('collapsed');

            if (headerLeftSection) headerLeftSection.style.setProperty('width', 'auto', 'important');
            if (sidebarLogo) sidebarLogo.classList.add('hidden');
            if (mainLogo) {
                mainLogo.classList.remove('hidden');
                mainLogo.classList.add('flex');
            }

            setTimeout(function() {
                var dropdownContainers = sidebar.querySelectorAll('[x-data]');
                dropdownContainers.forEach(function(container) {
                    if (container._x_dataStack && container._x_dataStack[0]) {
                        container._x_dataStack[0].open = false;
                    }
                });
            }, 0);
        }

        function setExpandedState() {
            if (isMobile()) return;

            sidebar.style.setProperty('width', '240px', 'important');
            sidebar.classList.remove('collapsed');

            if (headerLeftSection) headerLeftSection.style.setProperty('width', '240px', 'important');
            if (sidebarLogo) sidebarLogo.classList.remove('hidden');
            if (mainLogo) {
                mainLogo.classList.add('hidden');
                mainLogo.classList.remove('flex');
            }
        }

        function openMobileDrawer() {
            sidebar.classList.add('sidebar-mobile-open');
            if (mobileOverlay) mobileOverlay.classList.add('active');
            document.body.style.overflow = 'hidden';
        }

        function closeMobileDrawer() {
            sidebar.classList.remove('sidebar-mobile-open');
            if (mobileOverlay) mobileOverlay.classList.remove('active');
            document.body.style.overflow = '';
        }

        function initializeSidebar() {
            document.documentElement.classList.remove('sidebar-collapsed-init');

            if (isMobile()) {
                sidebar.classList.remove('collapsed');
                sidebar.style.setProperty('width', '280px', 'important');
            } else {
                var savedState = localStorage.getItem('sidebar-collapsed');

                if (isTablet() && savedState !== 'false') {
                    setCollapsedState();
                } else if (savedState === 'true') {
                    setCollapsedState();
                } else {
                    setExpandedState();
                }
            }
        }

        var resizeTimer;
        var preventResizeReinit = false;

        // Hamburger toggle
        sidebarToggleBtn.addEventListener('click', function() {
            if (isMobile()) {
                if (sidebar.classList.contains('sidebar-mobile-open')) {
                    closeMobileDrawer();
                } else {
                    openMobileDrawer();
                }
            } else {
                if (sidebar.classList.contains('collapsed')) {
                    localStorage.setItem('sidebar-collapsed', 'false');
                    setExpandedState();
                } else {
                    localStorage.setItem('sidebar-collapsed', 'true');
                    setCollapsedState();
                }
                preventResizeReinit = true;
                setTimeout(function() { preventResizeReinit = false; }, 1000);
            }
        });

        // Window resize
        window.addEventListener('resize', function() {
            clearTimeout(resizeTimer);
            resizeTimer = setTimeout(function() {
                if (preventResizeReinit) {
                    preventResizeReinit = false;
                    return;
                }

                if (!isMobile() && sidebar.classList.contains('sidebar-mobile-open')) {
                    closeMobileDrawer();
                }
                initializeSidebar();
            }, 250);
        });

        // Make expand function available for dropdown clicks
        expandSidebarFunc = function() {
            localStorage.setItem('sidebar-collapsed', 'false');
            setExpandedState();
            preventResizeReinit = true;
            setTimeout(function() { preventResizeReinit = false; }, 1000);
        };

        // Close mobile drawer on overlay click
        if (mobileOverlay) {
            mobileOverlay.addEventListener('click', function() {
                if (isMobile()) closeMobileDrawer();
            });
        }

        // Close mobile drawer when navigating
        var sidebarLinks = sidebar.querySelectorAll('a');
        sidebarLinks.forEach(function(link) {
            link.addEventListener('click', function() {
                if (isMobile() && sidebar.classList.contains('sidebar-mobile-open')) {
                    setTimeout(function() { closeMobileDrawer(); }, 100);
                }
            });
        });

        initializeSidebar();
    }

    // ============================================
    // File Attachments (Data URL Detection)
    // ============================================

    var dataUrlPattern = /^data:([^;,]+)?(;base64)?,(.+)$/;

    function isDataUrl(str) {
        if (typeof str !== 'string') return false;
        return dataUrlPattern.test(str);
    }

    function parseDataUrl(dataUrl) {
        var match = dataUrl.match(dataUrlPattern);
        if (!match) return null;

        var mimeType = match[1] || 'application/octet-stream';
        var isBase64 = !!match[2];
        var data = match[3];

        var size = 0;
        if (isBase64) {
            size = Math.round(data.length * 0.75);
        } else {
            size = data.length;
        }

        return {
            mimeType: mimeType,
            isBase64: isBase64,
            data: data,
            size: size,
            dataUrl: dataUrl
        };
    }

    // Check if an object is a file object with separate data, content_type fields
    // Format: { data: "base64...", content_type: "image/png", name?: "file.png", size?: 12345 }
    function isFileObject(obj) {
        if (!obj || typeof obj !== 'object') return false;
        // Must have data and content_type fields
        if (typeof obj.data !== 'string' || typeof obj.content_type !== 'string') return false;
        // data should be base64 (no data: prefix) - check for reasonable base64 content
        // Avoid matching if data is too short or looks like a regular string
        if (obj.data.length < 100) return false;
        // Should not start with 'data:' (that's the other format)
        if (obj.data.indexOf('data:') === 0) return false;
        return true;
    }

    // Parse a file object into our standard format
    function parseFileObject(fileObj) {
        var mimeType = fileObj.content_type || 'application/octet-stream';
        var data = fileObj.data;

        // Calculate size from base64 if not provided
        var size = fileObj.size;
        if (size === undefined || size === null) {
            size = Math.round(data.length * 0.75);
        }

        // Reconstruct the data URL for display/download
        var dataUrl = 'data:' + mimeType + ';base64,' + data;

        return {
            mimeType: mimeType,
            isBase64: true,
            data: data,
            size: size,
            dataUrl: dataUrl,
            fileName: fileObj.name || null,
            filePath: fileObj.path || null
        };
    }

    function isImageMimeType(mimeType) {
        return mimeType && mimeType.startsWith('image/');
    }

    function getFileTypeLabel(mimeType) {
        var typeMap = {
            'image/png': 'PNG Image',
            'image/jpeg': 'JPEG Image',
            'image/jpg': 'JPEG Image',
            'image/gif': 'GIF Image',
            'image/webp': 'WebP Image',
            'image/svg+xml': 'SVG Image',
            'image/bmp': 'BMP Image',
            'image/tiff': 'TIFF Image',
            'application/pdf': 'PDF Document',
            'application/json': 'JSON File',
            'text/plain': 'Text File',
            'text/html': 'HTML File',
            'text/csv': 'CSV File',
            'audio/mpeg': 'MP3 Audio',
            'audio/wav': 'WAV Audio',
            'video/mp4': 'MP4 Video',
            'video/webm': 'WebM Video'
        };
        return typeMap[mimeType] || (mimeType ? mimeType.split('/')[1].toUpperCase() : 'File');
    }

    function formatFileSize(bytes) {
        if (bytes < 1024) return bytes + ' B';
        if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB';
        return (bytes / (1024 * 1024)).toFixed(1) + ' MB';
    }

    function findDataUrls(value, path) {
        if (path === undefined) path = '';
        var results = [];

        if (typeof value === 'string' && isDataUrl(value)) {
            // Format 1: data URL string (data:mime/type;base64,...)
            var parsed = parseDataUrl(value);
            if (parsed) {
                parsed.path = path;
                results.push(parsed);
            }
        } else if (isFileObject(value)) {
            // Format 2: file object with { data, content_type, name?, size? }
            var parsed = parseFileObject(value);
            if (parsed) {
                parsed.path = path;
                results.push(parsed);
            }
        } else if (Array.isArray(value)) {
            value.forEach(function(item, index) {
                var newPath = path ? path + '[' + index + ']' : '[' + index + ']';
                var found = findDataUrls(item, newPath);
                for (var i = 0; i < found.length; i++) {
                    results.push(found[i]);
                }
            });
        } else if (value && typeof value === 'object') {
            if (value['$val'] !== undefined) {
                var found = findDataUrls(value['$val'], path);
                for (var i = 0; i < found.length; i++) {
                    results.push(found[i]);
                }
            } else {
                Object.keys(value).forEach(function(key) {
                    var newPath = path ? path + '.' + key : key;
                    var found = findDataUrls(value[key], newPath);
                    for (var i = 0; i < found.length; i++) {
                        results.push(found[i]);
                    }
                });
            }
        }

        return results;
    }

    function renderFileAttachments(dataUrls, containerId) {
        if (!dataUrls || dataUrls.length === 0) return '';

        var attachmentsHtml = dataUrls.map(function(file, index) {
            var isImage = isImageMimeType(file.mimeType);
            var fileType = getFileTypeLabel(file.mimeType);
            var fileSize = formatFileSize(file.size);
            var pathInfo = file.path ? ' • ' + file.path : '';
            // Use fileName if available (from file object format), otherwise generate from type
            var downloadName = file.fileName || (fileType.toLowerCase().replace(/ /g, '-') + '.' + getExtensionFromMimeType(file.mimeType));
            // Display name: use fileName if available, otherwise show file type
            var displayName = file.fileName || fileType;

            if (isImage) {
                return '<div class="file-attachment" data-file-index="' + index + '">' +
                    '<img src="' + file.dataUrl + '" alt="' + displayName + '" class="file-attachment-thumbnail" onclick="openLightbox(\'' + file.dataUrl + '\')" title="Click to preview" />' +
                    '<div class="file-attachment-info">' +
                        '<span class="file-attachment-type" title="' + escapeHtml(displayName) + '">' + escapeHtml(truncateFileName(displayName, 24)) + '</span>' +
                        '<span class="file-attachment-size">' + fileSize + pathInfo + '</span>' +
                    '</div>' +
                    '<div class="file-attachment-actions">' +
                        '<button class="file-attachment-btn" onclick="downloadDataUrl(\'' + file.dataUrl + '\', \'' + escapeHtml(downloadName) + '\')" title="Download">' +
                            '<svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 16v1a3 3 0 003 3h10a3 3 0 003-3v-1m-4-4l-4 4m0 0l-4-4m4 4V4"></path></svg>' +
                        '</button>' +
                    '</div>' +
                '</div>';
            } else {
                return '<div class="file-attachment" data-file-index="' + index + '">' +
                    '<div class="file-attachment-icon">' +
                        '<svg class="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z"></path></svg>' +
                    '</div>' +
                    '<div class="file-attachment-info">' +
                        '<span class="file-attachment-type" title="' + escapeHtml(displayName) + '">' + escapeHtml(truncateFileName(displayName, 24)) + '</span>' +
                        '<span class="file-attachment-size">' + fileSize + pathInfo + '</span>' +
                    '</div>' +
                    '<div class="file-attachment-actions">' +
                        '<button class="file-attachment-btn" onclick="downloadDataUrl(\'' + file.dataUrl + '\', \'' + escapeHtml(downloadName) + '\')" title="Download">' +
                            '<svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 16v1a3 3 0 003 3h10a3 3 0 003-3v-1m-4-4l-4 4m0 0l-4-4m4 4V4"></path></svg>' +
                        '</button>' +
                    '</div>' +
                '</div>';
            }
        }).join('');

        return '<div class="file-attachments">' + attachmentsHtml + '</div>';
    }

    function truncateFileName(name, maxLength) {
        if (!name || name.length <= maxLength) return name;
        var ext = '';
        var dotIndex = name.lastIndexOf('.');
        if (dotIndex > 0) {
            ext = name.substring(dotIndex);
            name = name.substring(0, dotIndex);
        }
        var availableLength = maxLength - ext.length - 3; // 3 for '...'
        if (availableLength < 5) availableLength = 5;
        return name.substring(0, availableLength) + '...' + ext;
    }

    function escapeHtml(str) {
        if (!str) return '';
        return str.replace(/&/g, '&amp;')
                  .replace(/</g, '&lt;')
                  .replace(/>/g, '&gt;')
                  .replace(/"/g, '&quot;')
                  .replace(/'/g, '&#39;');
    }

    // Lightbox state
    var currentLightboxUrl = '';

    function openLightbox(dataUrl) {
        currentLightboxUrl = dataUrl;
        var lightbox = document.getElementById('image-lightbox');
        var img = document.getElementById('lightbox-image');
        if (lightbox && img) {
            img.src = dataUrl;
            lightbox.classList.add('active');
            document.body.style.overflow = 'hidden';
        }
    }

    function closeLightbox(event) {
        if (event && event.target !== event.currentTarget) return;
        var lightbox = document.getElementById('image-lightbox');
        if (lightbox) {
            lightbox.classList.remove('active');
            document.body.style.overflow = '';
        }
    }

    function downloadLightboxImage() {
        if (currentLightboxUrl) {
            downloadDataUrl(currentLightboxUrl, 'image');
        }
    }

    function getExtensionFromMimeType(mimeType) {
        var extMap = {
            'image/png': 'png',
            'image/jpeg': 'jpg',
            'image/jpg': 'jpg',
            'image/gif': 'gif',
            'image/webp': 'webp',
            'image/svg+xml': 'svg',
            'image/bmp': 'bmp',
            'image/tiff': 'tiff',
            'application/pdf': 'pdf',
            'application/json': 'json',
            'text/plain': 'txt',
            'text/html': 'html',
            'text/csv': 'csv',
            'audio/mpeg': 'mp3',
            'audio/wav': 'wav',
            'video/mp4': 'mp4',
            'video/webm': 'webm'
        };
        return extMap[mimeType] || 'bin';
    }

    function downloadDataUrl(dataUrl, filename) {
        var parsed = parseDataUrl(dataUrl);
        if (!parsed) return;

        var fullFilename = filename || 'file';
        // Only add extension if filename doesn't already have one
        if (fullFilename.indexOf('.') === -1) {
            var ext = getExtensionFromMimeType(parsed.mimeType);
            fullFilename = fullFilename + '.' + ext;
        }

        var link = document.createElement('a');
        link.href = dataUrl;
        link.download = fullFilename;
        document.body.appendChild(link);
        link.click();
        document.body.removeChild(link);
    }

    // ============================================
    // Initialization
    // ============================================

    function init() {
        initUuidDisplay();
        initSidebar();

        // Set up event delegation for UUID copy
        document.addEventListener('click', handleUuidCopyClick);

        // Keyboard shortcuts for modals
        document.addEventListener('keydown', function(e) {
            if (e.key === 'Escape') {
                closeContentModal();
                closeSourceBrowser();
                closeLightbox();
            }
            // Ctrl+A or Cmd+A in content modal selects modal content only
            if ((e.ctrlKey || e.metaKey) && e.key === 'a' && isContentModalOpen()) {
                e.preventDefault();
                selectModalContent();
            }
            // Ctrl+C or Cmd+C in content modal copies modal content
            if ((e.ctrlKey || e.metaKey) && e.key === 'c' && isContentModalOpen()) {
                // Only intercept if no text is currently selected (let natural copy work otherwise)
                var selection = window.getSelection();
                if (!selection || selection.toString().length === 0) {
                    e.preventDefault();
                    copyModalContent();
                }
            }
            if ((e.ctrlKey || e.metaKey) && e.key === 'c' && isSourceBrowserOpen()) {
                var sourceSelection = window.getSelection();
                if (!sourceSelection || sourceSelection.toString().length === 0) {
                    e.preventDefault();
                    copySourceBrowserContent();
                }
            }
        });
    }

    // Run init on DOMContentLoaded
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', init);
    } else {
        init();
    }

    // ============================================
    // Export to Global Scope
    // ============================================

    // Value formatters
    window.formatAsHotLiteral = formatAsHotLiteral;
    window.formatAsJson = formatAsJson;
    window.isValidHotIdentifier = isValidHotIdentifier;

    // Content modal
    window.initModalFormat = initModalFormat;
    window.openContentModal = openContentModal;
    window.openContentModalWithFormats = openContentModalWithFormats;
    window.openContentModalFromElement = openContentModalFromElement;
    window.openContentModalFromScript = openContentModalFromScript;
    window.openPlainTextModal = openPlainTextModal;
    window.openPlainTextModalFromScript = openPlainTextModalFromScript;
    window.openContentModalWithData = openContentModalWithData;
    window.openEventDataFromElement = openEventDataFromElement;
    window.switchModalFormat = switchModalFormat;
    window.closeContentModal = closeContentModal;
    window.copyModalContent = copyModalContent;
    window.selectModalContent = selectModalContent;
    window.isContentModalOpen = isContentModalOpen;
    window.decodeHtmlEntities = decodeHtmlEntities;

    // Source browser
    window.openSourceBrowser = openSourceBrowser;
    window.openSourceBrowserFromElement = openSourceBrowserFromElement;
    window.closeSourceBrowser = closeSourceBrowser;
    window.copySourceBrowserContent = copySourceBrowserContent;
    window.filterSourceBrowserFiles = filterSourceBrowserFiles;
    window.searchSourceBrowserFiles = searchSourceBrowserFiles;
    window.toggleSourceBrowserCaseSensitive = toggleSourceBrowserCaseSensitive;
    window.toggleSourceBrowserRegex = toggleSourceBrowserRegex;

    // UUID utilities
    window.copyToClipboard = copyToClipboard;
    window.truncateUuid = truncateUuid;

    // Sidebar
    window.handleSidebarDropdownClick = handleSidebarDropdownClick;

    // File attachments
    window.isDataUrl = isDataUrl;
    window.parseDataUrl = parseDataUrl;
    window.findDataUrls = findDataUrls;
    window.renderFileAttachments = renderFileAttachments;
    window.openLightbox = openLightbox;
    window.closeLightbox = closeLightbox;
    window.downloadLightboxImage = downloadLightboxImage;
    window.downloadDataUrl = downloadDataUrl;

})();
