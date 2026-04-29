#!/bin/bash

# Script to count lines of code in Rust crates and Hot files
# Usage: ./scripts/loc.sh [directory]
# If no directory is provided, uses current directory

# Set directory to first argument or current directory
DIR="${1:-.}"

# Check if directory exists
if [ ! -d "$DIR" ]; then
    echo "Error: Directory '$DIR' does not exist"
    exit 1
fi

echo "Counting lines of code in: $DIR"
echo

# Count lines in Rust crates using cloc
if [ -d "$DIR/crates" ]; then
    echo "=== Rust crates (cloc) ==="
    cloc --timeout 0 "$DIR/crates"
    echo
else
    echo "No 'crates' directory found in $DIR"
    echo
fi

# Count lines in Hot files
echo "=== Hot files (.hot) ==="

# Count only files (avoid directories named .hot and exclude all .hot/ and target/ directories)
HOT_FILE_COUNT=$(find "$DIR" -name ".hot" -type d -prune -o -path "$DIR/target" -prune -o -type f -name "*.hot" -print 2>/dev/null | wc -l | tr -d ' ')

if [ "$HOT_FILE_COUNT" = "0" ] || [ -z "$HOT_FILE_COUNT" ]; then
    echo "No .hot files found in $DIR"
else
    # Group by src and test roots and print tables and totals
    find "$DIR" -name ".hot" -type d -prune -o -path "$DIR/target" -prune -o -type f -name "*.hot" -print0 2>/dev/null | \
    xargs -0 awk '
        function header(title) {
            printf "%s\n", "-------------------------------------------------------------------------------";
            printf "%s\n", title;
            printf "%s\n", "-------------------------------------------------------------------------------";
            printf "%-35s %10s %10s %10s %10s\n", "Path", "files", "blank", "comment", "code";
            printf "%s\n", "-------------------------------------------------------------------------------";
        }
        function row(path, f, b, c, co) {
            sp = shorten_path(path, 35);
            printf "%-35s %10d %10d %10d %10d\n", sp, f+0, b+0, c+0, co+0;
        }
        function sort_keys(a, n,    i, j, key) {
            for (i = 2; i <= n; i++) {
                key = a[i];
                j = i - 1;
                while (j >= 1 && a[j] > key) {
                    a[j+1] = a[j];
                    j--;
                }
                a[j+1] = key;
            }
        }
        function shorten_path(p, max) {
            if (length(p) <= max) return p;
            return "…" substr(p, length(p) - (max - 2));
        }
        FNR==1 {
            file = FILENAME;
            if (file ~ /\/src\//) {
                n = split(file, a, /\/src\//);
                group = a[1] "/src";
                type[group] = "src";
                groups[group] = 1;
                files[group]++;
                current = group;
            } else if (file ~ /\/test\//) {
                n = split(file, a, /\/test\//);
                group = a[1] "/test";
                type[group] = "test";
                groups[group] = 1;
                files[group]++;
                current = group;
            } else {
                # Root-level .hot files (no /src/ or /test/ in path)
                n = split(file, a, /\//);
                dir = "";
                for (i = 1; i < n; i++) dir = dir (i > 1 ? "/" : "") a[i];
                if (dir == "" || dir == ".") dir = "(root)";
                group = dir;
                type[group] = "root";
                groups[group] = 1;
                files[group]++;
                current = group;
            }
        }
        {
            if (current == "") next;
            if ($0 ~ /^[[:space:]]*$/) { blank[current]++; next }
            if ($0 ~ /^[[:space:]]*\/\//) { comment[current]++; next }
            code[current]++
        }
        END {
            sf = sb = sc = scc = 0;
            tf = tb = tc = tcc = 0;
            rf = rb = rc = rcc = 0;

            root_n = 0;
            for (g in groups) if (type[g] == "root") root_keys[++root_n] = g;
            if (root_n > 0) {
                header("Hot root paths");
                sort_keys(root_keys, root_n);
                for (i = 1; i <= root_n; i++) {
                    g = root_keys[i];
                    row(g, files[g], blank[g], comment[g], code[g]);
                    rf += files[g]; rb += blank[g]; rc += comment[g]; rcc += code[g];
                }
                printf "%s\n\n", "-------------------------------------------------------------------------------";
            }

            src_n = 0;
            for (g in groups) if (type[g] == "src") src_keys[++src_n] = g;
            if (src_n > 0) {
                header("Hot src paths");
                sort_keys(src_keys, src_n);
                for (i = 1; i <= src_n; i++) {
                    g = src_keys[i];
                    row(g, files[g], blank[g], comment[g], code[g]);
                    sf += files[g]; sb += blank[g]; sc += comment[g]; scc += code[g];
                }
                printf "%s\n\n", "-------------------------------------------------------------------------------";
            }

            test_n = 0;
            for (g in groups) if (type[g] == "test") test_keys[++test_n] = g;
            if (test_n > 0) {
                header("Hot test paths");
                sort_keys(test_keys, test_n);
                for (i = 1; i <= test_n; i++) {
                    g = test_keys[i];
                    row(g, files[g], blank[g], comment[g], code[g]);
                    tf += files[g]; tb += blank[g]; tc += comment[g]; tcc += code[g];
                }
                printf "%s\n\n", "-------------------------------------------------------------------------------";
            }

            total_f = rf + sf + tf;
            total_b = rb + sb + tb;
            total_cmt = rc + sc + tc;
            total_code = rcc + scc + tcc;

            printf "%s\n", "-------------------------------------------------------------------------------";
            printf "%-35s %10s %10s %10s %10s\n", "Total (root + src + test)", "files", "blank", "comment", "code";
            printf "%s\n", "-------------------------------------------------------------------------------";
            row("SUM:", total_f, total_b, total_cmt, total_code);
            printf "%s\n\n", "-------------------------------------------------------------------------------";
        }
    '
fi
