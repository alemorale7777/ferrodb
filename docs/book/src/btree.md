# The B+-tree

The B+-tree is the data structure that turns a heap of pages into an **ordered map**: keys stored in
sorted order, with logarithmic point lookups and efficient ordered range scans. Every table in
ferrodb is a B+-tree keyed by its primary key (or a hidden row id).

## Shape

A B+-tree is a balanced tree of pages:

- **Internal nodes** hold separator keys and child page pointers. They exist only to route a search
  to the right leaf.
- **Leaf nodes** hold the actual `(key, value)` entries, in sorted order, and each leaf points to
  its **right sibling** — so once you find the first matching leaf, an ordered scan is just walking
  the sibling chain.

All leaves are at the same depth; the tree stays balanced by construction.

## Point lookup

To find a key, start at the root and at each internal node binary-search the separators to pick the
child to descend into, until you reach a leaf; then binary-search the leaf. The number of page
fetches is the tree height — `O(log n)`. This is the operation behind every PK `IndexSeek`.

## Insert, split, and root growth

Insertion finds the target leaf and adds the entry. If the leaf overflows its 4 KiB, it **splits**:
the entries are divided between the old leaf and a new one, and the split's separator key is pushed
up into the parent. If the parent overflows, it splits too, and so on up the tree. If the *root*
splits, a brand-new root is created one level up — this is the only way the tree grows taller, and
it is what keeps every leaf at the same depth.

This split-on-overflow behaviour is exactly what the WASM visualizer (Chapter 9) animates: insert
enough rows and you watch a single-leaf tree become a root over two leaves, then three, and so on.

## Range scans

Because leaves are sorted and chained, an ordered range scan is: descend to the leaf containing the
lower bound, then walk right along the sibling chain emitting entries until the upper bound is
crossed. `scan(lo, hi)` treats `lo` as inclusive and `hi` as exclusive. The optimizer's
`IndexRange` access path (Chapter 7) is a thin wrapper over exactly this call — it seeks straight to
the starting leaf rather than scanning the whole table.

## Overflow chains

A value larger than a page cannot fit in a leaf cell. ferrodb stores such values in a chain of
**overflow pages**: the leaf holds a small stub pointing at the first overflow page, and each
overflow page points to the next. A 20 KB value round-trips transparently through this chain.

## Proving it correct

A B+-tree is fiddly enough that correctness must be *demonstrated*, not asserted. ferrodb's core
test runs hundreds of randomized insert/delete sequences against a `std::collections::BTreeMap`
model and asserts the two agree on every operation, alongside a 2 000-key split-stress test and the
20 KB overflow round-trip. If the tree ever diverges from the model, the property test shrinks the
failing sequence to a minimal reproducer.
