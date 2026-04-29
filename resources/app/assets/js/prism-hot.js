/**
 * Prism.js syntax highlighting for Hot language
 * Adapted from the VSCode TextMate grammar
 */

(function (Prism) {
	Prism.languages.hot = {
		'comment': {
			pattern: /\/\/.*/,
			greedy: true
		},
		'meta-annotation': [
		{
			// Meta annotation with brackets: meta [...]
			pattern: /\bmeta\s*\[(?:[^\]"]*|"""[\s\S]*?"""|"(?:[^"\\]|\\.)*")*\]/,
			greedy: true,
			inside: {
				'keyword': /^meta/,
				'punctuation': /\[|\]$/,
				'string-triple': {
					pattern: /"""[\s\S]*?"""/,
					greedy: true,
					alias: 'string'
				},
				'string': {
					pattern: /"(?:[^"\\]|\\.)*"/,
					greedy: true
				},
					'number': /-?\d+(?:\.\d+)?/,
					'boolean': /(?<![a-zA-Z0-9_-])(?:true|false)(?![a-zA-Z0-9_-])/,
					'null': /(?<![a-zA-Z0-9_-])null(?![a-zA-Z0-9_-])/,
					'punctuation': /[,:]/
				}
			},
		{
			// Meta annotation with braces: meta {...}
			pattern: /\bmeta\s*\{(?:[^}"]*|"""[\s\S]*?"""|"(?:[^"\\]|\\.)*")*\}/,
			greedy: true,
			inside: {
				'keyword': /^meta/,
				'punctuation': /\{|\}$/,
				'string-triple': {
					pattern: /"""[\s\S]*?"""/,
					greedy: true,
					alias: 'string'
				},
				'property': {
					pattern: /"(?:[^"\\]|\\.)*"(?=\s*:)/,
					greedy: true
				},
				'string': {
					pattern: /"(?:[^"\\]|\\.)*"/,
					greedy: true
				},
					'number': /-?\d+(?:\.\d+)?/,
					'boolean': /(?<![a-zA-Z0-9_-])(?:true|false)(?![a-zA-Z0-9_-])/,
					'null': /(?<![a-zA-Z0-9_-])null(?![a-zA-Z0-9_-])/,
					'punctuation': /[,:]/
				}
			},
		{
			// Meta annotation triple-quote string: meta """..."""
			pattern: /\bmeta\s+"""[\s\S]*?"""/,
			greedy: true,
			inside: {
				'keyword': /^meta/,
				'string': /"""[\s\S]*?"""/
			}
		},
		{
			// Meta annotation string: meta "..."
			pattern: /\bmeta\s+"(?:[^"\\]|\\.)*"/,
			greedy: true,
			inside: {
				'keyword': /^meta/,
				'string': /"(?:[^"\\]|\\.)*"/
			}
		},
			{
				// Meta annotation number: meta 123
				pattern: /\bmeta\s+\d+/,
				greedy: true,
				inside: {
					'keyword': /^meta/,
					'number': /\d+/
				}
			}
		],
	// Triple-backtick templates (indent-aware, interpolation, no escape processing)
	// Must come before single-backtick template to prevent ``` being consumed as three single backticks
	'string-template-triple': {
		pattern: /```[\s\S]*?```/,
		greedy: true
	},
	// Template literals - basic pattern, interpolation added after language is defined
	'string-template': {
		pattern: /`(?:[^`\\$]|\\.|\$(?!\{)|\$\{[^}]*\})*`/,
		greedy: true
	},
	// Triple-quote strings (raw multi-line, no escape processing)
	'string-triple': {
		pattern: /"""[\s\S]*?"""/,
		greedy: true,
		alias: 'string'
	},
	// Property name (quoted map key) - must come before regular string
	'property': {
		pattern: /"(?:[^"\\]|\\.)*"(?=\s*:)/,
		greedy: true
	},
	// Unquoted map key - identifier followed by : (not ::)
	// Must come before keyword to prevent highlighting map keys as keywords
	'map-key': {
		pattern: /\b[a-zA-Z_][a-zA-Z0-9_\-?!]*(?=\s*:(?!:))/,
		alias: 'property'
	},
	'string': {
		pattern: /"(?:[^"\\]|\\.)*"/,
		greedy: true
	},
	// Namespaced function call - MUST come before keyword to prevent highlighting keywords in namespace paths
	// e.g., ::hot::type/Result should not highlight "type" as keyword
	'namespaced-function': {
		pattern: /::[a-zA-Z_][a-zA-Z0-9_\-?!]*(?:::[a-zA-Z_][a-zA-Z0-9_\-?!]*)*\/[a-zA-Z_][a-zA-Z0-9_\-?!]*/,
		greedy: true,
		inside: {
			'namespace': {
				pattern: /::[a-zA-Z_][a-zA-Z0-9_\-?!]*(?:::[a-zA-Z_][a-zA-Z0-9_\-?!]*)*/
			},
			'punctuation': /\//,
			'function': /[a-zA-Z_][a-zA-Z0-9_\-?!]*$/
		}
	},
	// Standalone namespace reference (without function) - MUST come before keyword
	'namespace': {
		pattern: /::[a-zA-Z_][a-zA-Z0-9_\-?!]*(?:::[a-zA-Z_][a-zA-Z0-9_\-?!]*)*/,
		greedy: true
	},
	// Keywords: fn, type, ns, meta are declarations; lazy, do are control; serial, parallel, cond, cond-all, match, match-all are flow
	// Note: '->' (implements) is handled separately
	// Use explicit lookbehind/lookahead to prevent matching keywords within hyphenated identifiers like 'is-null' or 'some-type'
	'keyword': [
		/(?<![a-zA-Z0-9_-])(?:meta|type|enum|fn|ns|lazy|do|serial|parallel|cond-all|cond|match-all|match)(?![a-zA-Z0-9_-])/,
		/\|>/  // Pipe operator
	],
		'boolean': /(?<![a-zA-Z0-9_-])(?:true|false)(?![a-zA-Z0-9_-])/,
		'null': /(?<![a-zA-Z0-9_-])null(?![a-zA-Z0-9_-])/,
		'special-identifier': {
			pattern: /\$[a-zA-Z_][a-zA-Z0-9_\-?!]*/,
			greedy: true
		},
		// All identifier patterns must come before number to prevent splitting identifiers like Point2D or var2
		// Using explicit lookbehind/lookahead instead of \b for better digit handling
		'type-constructor': {
			pattern: /(?<![a-zA-Z0-9_])[A-Z][a-zA-Z0-9_\-?!]*(?=\s*\()/,
			greedy: true
		},
		'function': {
			pattern: /(?<![a-zA-Z0-9_])[a-zA-Z_][a-zA-Z0-9_\-?!]*(?=\s*\()/,
			greedy: true
		},
		'class-name': {
			pattern: /(?<![a-zA-Z0-9_])[A-Z][a-zA-Z0-9_\-?!]*(?![a-zA-Z0-9_])/,
			greedy: true
		},
		'placeholder': {
			pattern: /%(?:\d+)?/,
			alias: 'variable'
		},
		'identifier': {
			pattern: /(?<![a-zA-Z0-9_])[a-z_][a-zA-Z0-9_\-?!]*(?![a-zA-Z0-9_])/,
			greedy: true
		},
		'number': /(?<![a-zA-Z_\d])-?\d+(?:\.\d+)?(?![a-zA-Z_])/,
		'flow-modifier': /\|(?:all|one|map|vec)\b/,
		'punctuation': /[{}[\](),.:;]/
	};

	// Now add interpolation support to string-template (must be done after language is defined)
	Prism.languages.hot['string-template'] = {
		pattern: /`(?:[^`\\$]|\\.|\$(?!\{)|\$\{[^}]*\})*`/,
		greedy: true,
		inside: {
			'interpolation': {
				pattern: /\$\{[^}]*\}/,
				inside: {
					'interpolation-punctuation': {
						pattern: /^\$\{|\}$/,
						alias: 'punctuation'
					},
					rest: Prism.languages.hot
				}
			},
			'template-string': {
				pattern: /[\s\S]+/,
				alias: 'string'
			}
		}
	};

	// Add interpolation support to triple-backtick templates
	Prism.languages.hot['string-template-triple'] = {
		pattern: /```[\s\S]*?```/,
		greedy: true,
		inside: {
			'interpolation': {
				pattern: /\$\{[^}]*\}/,
				inside: {
					'interpolation-punctuation': {
						pattern: /^\$\{|\}$/,
						alias: 'punctuation'
					},
					rest: Prism.languages.hot
				}
			},
			'template-string': {
				pattern: /[\s\S]+/,
				alias: 'string'
			}
		}
	};

}(Prism));
