///Estimate the cost of an item.  This is usually in bytes.
///
/// The caches in this crate will cache up to a specified total cost, then begin
/// evicting entries which are least recently used.
pub trait EstimateCost {
    fn estimate_cost(&self) -> usize;
}
