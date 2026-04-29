#!/bin/bash
# doc-snippets.sh - Extract and inject Hot code snippets into documentation
#
# Usage:
#   ./scripts/doc-snippets.sh check     # Verify all snippets compile and referenced snippets exist
#   ./scripts/doc-snippets.sh test      # Run tests in doc example files
#   ./scripts/doc-snippets.sh extract   # Show all available snippets
#   ./scripts/doc-snippets.sh inject    # Generate markdown with injected snippets (stdout)
#   ./scripts/doc-snippets.sh build     # Build docs with injected snippets to resources/docs-built/
#   ./scripts/doc-snippets.sh eval      # Evaluate a snippet and show its result
#
# Snippet format in .hot files:
#   // @doc: snippet-name
#   <code here>
#   // @end: snippet-name
#
# Reference format in .md files:
#   {{snippet:filename#snippet-name}}        - Just show the code
#   {{snippet:filename#snippet-name:eval}}   - Show code AND evaluated result
#   e.g., {{snippet:flows#parallel-function}}
#   e.g., {{snippet:errors#safe-divide:eval}}

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
EXAMPLES_DIR="$PROJECT_ROOT/hot/test/docs"
DOCS_DIR="$PROJECT_ROOT/resources/docs"
DOCS_BUILT_DIR="$PROJECT_ROOT/resources/docs-built"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Extract a specific snippet from a .hot file
extract_snippet() {
    local file="$1"
    local name="$2"
    local in_snippet=false
    local found=false

    while IFS= read -r line; do
        if [[ "$line" =~ ^[[:space:]]*//[[:space:]]*@doc:[[:space:]]*${name}[[:space:]]*$ ]]; then
            in_snippet=true
            found=true
            continue
        fi
        if [[ "$line" =~ ^[[:space:]]*//[[:space:]]*@end:[[:space:]]*${name}[[:space:]]*$ ]]; then
            in_snippet=false
            continue
        fi
        if $in_snippet; then
            echo "$line"
        fi
    done < "$file"

    if ! $found; then
        return 1
    fi
}

# Evaluate a snippet and return its result
# For simple expressions, uses `hot eval` directly
# Snippet code should use fully qualified names or simple expressions
eval_snippet() {
    local file="$1"
    local name="$2"
    local snippet_code

    snippet_code=$(extract_snippet "$file" "$name") || return 1

    # Remove comments and get just the expression(s)
    # Strip leading/trailing whitespace and comment lines
    local expr
    expr=$(echo "$snippet_code" | grep -v '^[[:space:]]*//\|^[[:space:]]*$' | tail -1)

    if [[ -z "$expr" ]]; then
        return 1
    fi

    # Use hot eval to evaluate the expression
    cargo run --quiet -- eval "$expr" 2>/dev/null
}

# List all snippets in a file
list_snippets() {
    local file="$1"
    grep '// @doc:' "$file" 2>/dev/null | sed 's|.*// @doc: ||' | sed 's|[[:space:]]*$||' || true
}

# Command: extract - show all available snippets
cmd_extract() {
    echo "Available snippets:"
    echo "==================="

    for file in "$EXAMPLES_DIR"/*.hot; do
        if [[ -f "$file" ]]; then
            local basename=$(basename "$file" .hot)
            echo ""
            echo -e "${YELLOW}$basename:${NC}"
            for snippet in $(list_snippets "$file"); do
                echo "  - $basename#$snippet"
            done
        fi
    done
}

# Command: check - verify snippets compile and references are valid
cmd_check() {
    local errors=0

    echo "Checking doc examples..."
    echo "========================"

    # In CI, use the context file with placeholder API keys
    local ctx_args=""
    if [[ -n "$CI" ]] && [[ -f "$PROJECT_ROOT/hot/ci.ctx.hot" ]]; then
        ctx_args="--ctx hot/ci.ctx.hot"
    fi

    # 1. Run hot check on all example files
    echo ""
    echo -e "${YELLOW}Step 1: Syntax check${NC}"
    if cargo run --quiet -- check --with-tests true $ctx_args 2>/dev/null; then
        echo -e "${GREEN}✓ All example files pass syntax check${NC}"
    else
        echo -e "${RED}✗ Syntax errors in example files${NC}"
        cargo run -- check --with-tests true $ctx_args
        errors=$((errors + 1))
    fi

    # 2. Check that all referenced snippets exist
    echo ""
    echo -e "${YELLOW}Step 2: Verify snippet references${NC}"

    local missing=0
    while IFS= read -r ref; do
        # Extract file and snippet name from {{snippet:file#name}}
        if [[ "$ref" =~ \{\{snippet:([a-zA-Z0-9_-]+)#([a-zA-Z0-9_-]+)\}\} ]]; then
            local file="${BASH_REMATCH[1]}"
            local name="${BASH_REMATCH[2]}"
            local hot_file="$EXAMPLES_DIR/$file.hot"

            if [[ ! -f "$hot_file" ]]; then
                echo -e "${RED}✗ Missing file: $hot_file (referenced in docs)${NC}"
                missing=$((missing + 1))
            elif ! extract_snippet "$hot_file" "$name" > /dev/null 2>&1; then
                echo -e "${RED}✗ Missing snippet: $file#$name${NC}"
                missing=$((missing + 1))
            fi
        fi
    done < <(grep -rho '{{snippet:[^}]*}}' "$DOCS_DIR" 2>/dev/null || true)

    if [[ $missing -eq 0 ]]; then
        echo -e "${GREEN}✓ All snippet references are valid${NC}"
    else
        echo -e "${RED}✗ $missing missing snippet(s)${NC}"
        errors=$((errors + 1))
    fi

    echo ""
    if [[ $errors -eq 0 ]]; then
        echo -e "${GREEN}All checks passed!${NC}"
        return 0
    else
        echo -e "${RED}$errors check(s) failed${NC}"
        return 1
    fi
}

# Command: test - run tests in example files
cmd_test() {
    echo "Running doc example tests..."
    echo "============================="
    cargo run -- test "::hot::test::docs::"
}

# Command: inject - process a markdown file and inject snippets
inject_file() {
    local md_file="$1"

    while IFS= read -r line; do
        # Match {{snippet:file#name}} or {{snippet:file#name:eval}}
        if [[ "$line" =~ \{\{snippet:([a-zA-Z0-9_-]+)#([a-zA-Z0-9_-]+)(:eval)?\}\} ]]; then
            local file="${BASH_REMATCH[1]}"
            local name="${BASH_REMATCH[2]}"
            local do_eval="${BASH_REMATCH[3]}"
            local hot_file="$EXAMPLES_DIR/$file.hot"

            echo '```hot'
            if [[ -f "$hot_file" ]]; then
                local snippet_code
                snippet_code=$(extract_snippet "$hot_file" "$name")
                if [[ $? -eq 0 ]]; then
                    echo "$snippet_code"

                    # If :eval suffix present, also show the result
                    if [[ "$do_eval" == ":eval" ]]; then
                        local result
                        result=$(eval_snippet "$hot_file" "$name" 2>/dev/null)
                        if [[ -n "$result" ]]; then
                            echo "// → $result"
                        fi
                    fi
                else
                    echo "// ERROR: Snippet not found: $file#$name"
                fi
            else
                echo "// ERROR: File not found: $hot_file"
            fi
            echo '```'
        else
            echo "$line"
        fi
    done < "$md_file"
}

# Command: build - build all docs with injected snippets
cmd_build() {
    echo "Building documentation with snippets..."
    echo "========================================"

    rm -rf "$DOCS_BUILT_DIR"
    mkdir -p "$DOCS_BUILT_DIR"

    # Copy and process all markdown files
    find "$DOCS_DIR" -name "*.md" | while read -r md_file; do
        local rel_path="${md_file#$DOCS_DIR/}"
        local out_file="$DOCS_BUILT_DIR/$rel_path"
        local out_dir=$(dirname "$out_file")

        mkdir -p "$out_dir"
        inject_file "$md_file" > "$out_file"
        echo "  Processed: $rel_path"
    done

    echo ""
    echo -e "${GREEN}Documentation built to: $DOCS_BUILT_DIR${NC}"
}

# Command: eval - evaluate a specific snippet
cmd_eval() {
    local ref="$1"

    if [[ "$ref" =~ ^([a-zA-Z0-9_-]+)#([a-zA-Z0-9_-]+)$ ]]; then
        local file="${BASH_REMATCH[1]}"
        local name="${BASH_REMATCH[2]}"
        local hot_file="$EXAMPLES_DIR/$file.hot"

        if [[ ! -f "$hot_file" ]]; then
            echo -e "${RED}Error: File not found: $hot_file${NC}"
            exit 1
        fi

        echo "Evaluating: $file#$name"
        echo "========================"
        echo ""
        echo "Code:"
        extract_snippet "$hot_file" "$name" | sed 's/^/  /'
        echo ""
        echo "Result:"
        result=$(eval_snippet "$hot_file" "$name")
        if [[ -n "$result" ]]; then
            echo "  → $result"
        else
            echo "  (no output)"
        fi
    else
        echo "Usage: $0 eval <file#snippet>"
        echo "Example: $0 eval flows#serial-basic"
        exit 1
    fi
}

# Main
case "${1:-help}" in
    check)
        cmd_check
        ;;
    test)
        cmd_test
        ;;
    extract)
        cmd_extract
        ;;
    eval)
        cmd_eval "$2"
        ;;
    inject)
        if [[ -n "$2" ]]; then
            inject_file "$2"
        else
            echo "Usage: $0 inject <file.md>"
            exit 1
        fi
        ;;
    build)
        cmd_build
        ;;
    help|--help|-h)
        echo "Usage: $0 <command>"
        echo ""
        echo "Commands:"
        echo "  check    - Verify all snippets compile and references exist"
        echo "  test     - Run tests in doc example files"
        echo "  extract  - Show all available snippets"
        echo "  eval     - Evaluate a snippet and show result"
        echo "  inject   - Process a markdown file (outputs to stdout)"
        echo "  build    - Build docs with injected snippets"
        echo ""
        echo "Example snippet in .hot file:"
        echo "  // @doc: my-snippet"
        echo "  some-function fn () { ... }"
        echo "  // @end: my-snippet"
        echo ""
        echo "Reference in .md file:"
        echo "  {{snippet:filename#my-snippet}}       # code only"
        echo "  {{snippet:filename#my-snippet:eval}}  # code + evaluated result"
        ;;
    *)
        echo "Unknown command: $1"
        echo "Run '$0 help' for usage"
        exit 1
        ;;
esac
