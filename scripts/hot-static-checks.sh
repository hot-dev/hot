#!/bin/bash
# hot-static-checks.sh - Run Hot static checks and documentation snippet validation
#
# Usage:
#   ./scripts/hot-static-checks.sh check     # Run Hot checks, fmt verify, and doc snippets
#   ./scripts/hot-static-checks.sh fmt       # Verify all Hot source is formatted
#   ./scripts/hot-static-checks.sh test      # Run tests in doc example files
#   ./scripts/hot-static-checks.sh extract   # Show all available snippets
#   ./scripts/hot-static-checks.sh inject    # Generate markdown with injected snippets (stdout)
#   ./scripts/hot-static-checks.sh build     # Build docs with injected snippets to resources/docs-built/
#   ./scripts/hot-static-checks.sh eval      # Evaluate a snippet and show its result
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

# Command: check - run Hot static checks and verify doc snippets
cmd_check() {
    local errors=0

    echo "Running Hot static checks..."
    echo "============================"

    # In CI, use the context file with placeholder API keys
    local ctx_args=""
    if [[ -n "$CI" ]] && [[ -f "$PROJECT_ROOT/hot/ci.ctx.hot" ]]; then
        ctx_args="--ctx hot/ci.ctx.hot"
    fi

    # 1. Run hot check on the default project and doc example files.
    echo ""
    echo -e "${YELLOW}Step 1: Default project check${NC}"
    if cargo run --quiet -- check --with-tests true $ctx_args 2>/dev/null; then
        echo -e "${GREEN}✓ Default project and doc examples pass static check${NC}"
    else
        echo -e "${RED}✗ Default project static check failed${NC}"
        cargo run -- check --with-tests true $ctx_args
        errors=$((errors + 1))
    fi

    # 2. Run the aggregate package check so optional packages stay type-checked
    # without making the default hot-dev project depend on every package.
    echo ""
    echo -e "${YELLOW}Step 2: Package aggregate check${NC}"
    if cargo run --quiet -- check -p hot-pkg-all --with-tests true $ctx_args 2>/dev/null; then
        echo -e "${GREEN}✓ Package aggregate passes static check${NC}"
    else
        echo -e "${RED}✗ Package aggregate static check failed${NC}"
        cargo run -- check -p hot-pkg-all --with-tests true $ctx_args
        errors=$((errors + 1))
    fi

    # 3. Verify all Hot source is formatted.
    echo ""
    echo -e "${YELLOW}Step 3: Hot formatting${NC}"
    if cmd_fmt > /dev/null 2>&1; then
        echo -e "${GREEN}✓ All Hot source is formatted${NC}"
    else
        echo -e "${RED}✗ Hot source has formatting drift${NC}"
        cmd_fmt
        errors=$((errors + 1))
    fi

    # 4. Check that all referenced snippets exist
    echo ""
    echo -e "${YELLOW}Step 4: Verify snippet references${NC}"

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

# Command: fmt - verify all Hot source is formatted (no writes)
# `hot fmt --check` takes one path per invocation, so loop the source roots.
cmd_fmt() {
    local roots=("hot/pkg" "hot/src" "hot/test")
    local errors=0

    echo "Checking Hot formatting..."
    echo "=========================="
    for root in "${roots[@]}"; do
        local dir="$PROJECT_ROOT/$root"
        [[ -d "$dir" ]] || continue
        if cargo run --quiet -- fmt --check "$dir" > /dev/null 2>&1; then
            echo -e "${GREEN}✓ $root${NC}"
        else
            echo -e "${RED}✗ $root has unformatted files:${NC}"
            cargo run --quiet -- fmt --check "$dir" 2>&1 | grep -v "properly formatted" || true
            errors=$((errors + 1))
        fi
    done

    echo ""
    if [[ $errors -eq 0 ]]; then
        echo -e "${GREEN}All Hot source is formatted.${NC}"
        echo "(run 'cargo run -- fmt <dir>' to fix)"
        return 0
    else
        echo -e "${RED}$errors source root(s) have formatting drift — run 'cargo run -- fmt <dir>'${NC}"
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
    fmt)
        cmd_fmt
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
        echo "  check    - Run all static checks (project + package + fmt + snippets)"
        echo "  fmt      - Verify all Hot source is formatted (hot fmt --check)"
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
