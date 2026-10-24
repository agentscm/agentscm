// Progress as a little commit graph: a line, one node per step, the finished stretch filled, the current step ringed.
export function Trail({ i, n }: { i: number; n: number }) {
  const f = (k: number) => (n > 1 ? k / (n - 1) : 0)
  const pos = (k: number) => ({ left: `calc(var(--r) + ${f(k)} * (100% - 2 * var(--r)))` })
  return (
    <div className="tour-trail" aria-hidden="true">
      <span className="tour-trail-line" />
      <span className="tour-trail-fill" style={{ transform: `scaleX(${f(i)})` }} />
      {Array.from({ length: n }, (_, k) => (
        <span key={k} className={`tour-node${k <= i ? ' is-done' : ''}`} style={pos(k)} />
      ))}
      <span className="tour-trail-ring" style={{ transform: `translateX(${f(i) * 100}%)` }} />
    </div>
  )
}
