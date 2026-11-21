---
name: invoice-parser
description: Parse vendor invoices and extract line items.
license: Apache-2.0
compatibility: Requires Hot 3.0+
allowed-tools: "Read Glob Bash(git add:*)"
metadata:
  author: hot-tests
  version: "1.2.0"
  hot-when: "parse invoice, vendor bill"
  hot-modalities: "pdf, image"
  hot-requires: "currency-rules"
custom-ext: "kept-in-extra"
---

# Invoice Parser

1. Check `references/schema.md` for line-item fields.
2. Follow `examples/vendor-a.md` for multi-currency totals.
