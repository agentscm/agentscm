// One small line illustration per tour, drawn for the product: cursors, lanes, commits, merges.
// Strokes draw in once (pathLength=1 + dash offset, see tour.css); without motion they are simply drawn.
import type { CSSProperties, ReactNode } from 'react'
import type { TourKey } from './content.ts'

type Tone = 'ink' | 'soft' | 'brand' | 'bad' | 'ok'
/** A stroke. `i` staggers the draw-in; `dot` marks a filled node that pops instead of drawing. */
const P = ({ d, t = 'ink', i = 0, dot }: { d: string; t?: Tone; i?: number; dot?: boolean }) => (
  <path d={d} pathLength={1} className={`ln ${t}${dot ? ' dot' : ''}`} style={{ '--i': i } as CSSProperties} />
)
const C = ({ x, y, r = 4, ...rest }: { x: number; y: number; r?: number; t?: Tone; i?: number; dot?: boolean }) => (
  <P d={`M${x - r} ${y}a${r} ${r} 0 1 0 ${2 * r} 0a${r} ${r} 0 1 0 ${-2 * r} 0z`} {...rest} />
)
const Box = ({ x, y, w, h, ...rest }: { x: number; y: number; w: number; h: number; t?: Tone; i?: number; dot?: boolean }) => <P d={`M${x} ${y}h${w}v${h}h${-w}z`} {...rest} />

const lanes = [0, 1, 2, 3].map((k) => 14 + k * 54)
const cardsPerLane = [3, 2, 2, 1]

const ART: Record<string, () => ReactNode> = {
  // Two agents reach for one file; the file carries a conflict mark.
  now: () => (
    <>
      <P d="M44 16v22l6-5 5 11 4-2-5-10h8z" t="brand" />
      <P d="M64 38C82 44 92 44 104 42" t="brand" i={2} />
      <P d="M196 16v22l-6-5-5 11-4-2 5-10h-8z" i={1} />
      <P d="M176 38C158 44 148 44 136 42" i={3} />
      <P d="M106 14h20l8 8v38h-28zM126 14v8h8" i={2} />
      <P d="M112 30h10M112 36h14" t="soft" i={4} />
      <C x={120} y={48} r={5} t="bad" i={5} />
      <P d="M117.5 45.5l5 5M122.5 45.5l-5 5" t="bad" i={6} />
    </>
  ),
  // Four lanes, cards in each.
  work: () => (
    <>
      {lanes.map((x, k) => (
        <g key={x}>
          <Box x={x} y={8} w={46} h={56} i={k} />
          <P d={`M${x + 6} 16h16`} t="soft" i={k + 1} />
          {Array.from({ length: cardsPerLane[k]! }, (_, c) => (
            <Box key={c} x={x + 6} y={24 + c * 13} w={34} h={8} t={k === 1 && c === 0 ? 'brand' : 'ink'} i={k + c + 2} />
          ))}
        </g>
      ))}
    </>
  ),
  // Events down a timeline, newest on top.
  activity: () => (
    <>
      <P d="M72 6v60" t="soft" />
      <C x={72} y={16} t="brand" dot i={1} />
      <C x={72} y={36} i={2} />
      <C x={72} y={56} i={3} />
      <P d="M44 16h16M44 36h16M44 56h16" t="soft" i={1} />
      <P d="M86 14h58M86 20h28" t="brand" i={2} />
      <P d="M86 34h84M86 40h40" i={3} />
      <P d="M86 54h44M86 60h66" i={4} />
    </>
  ),
  // Steps fan out, rejoin, and end in a passing check.
  workflows: () => (
    <>
      <Box x={24} y={26} w={20} h={20} />
      <Box x={84} y={8} w={20} h={20} />
      <Box x={84} y={44} w={20} h={20} />
      <Box x={140} y={26} w={20} h={20} />
      <P d="M44 36C62 36 64 18 84 18M44 36C62 36 64 54 84 54M104 18C124 18 120 36 140 36M104 54C124 54 120 36 140 36M160 36h34" t="brand" i={1} />
      <C x={206} y={36} r={12} t="ok" i={3} />
      <P d="M200 36l4.5 4.5L213 31" t="ok" i={5} />
    </>
  ),
  // A clipboard holding a diff hunk.
  'change-record': () => (
    <>
      <P d="M84 12h72v54h-72zM108 8h24v8h-24z" />
      <P d="M92 28h20" t="soft" i={2} />
      <P d="M92 38h6M104 38h40" t="bad" i={3} />
      <P d="M92 48h6M95 45v6M104 48h48" t="ok" i={4} />
      <P d="M92 58h34" t="soft" i={5} />
    </>
  ),
  // A branch merges into main and lands.
  landed: () => (
    <>
      <P d="M16 22h208" t="soft" />
      <C x={32} y={22} i={1} />
      <C x={64} y={22} i={1} />
      <P d="M64 22C78 22 82 52 98 52H140C156 52 160 22 176 22" t="brand" i={2} />
      <C x={100} y={52} r={3.5} t="brand" dot i={3} />
      <C x={120} y={52} r={3.5} t="brand" dot i={4} />
      <C x={140} y={52} r={3.5} t="brand" dot i={5} />
      <C x={200} y={22} r={9} t="ok" i={6} />
      <P d="M195.5 22l3 3 6-6" t="ok" i={7} />
    </>
  ),
  // An open book of steps, some ticked.
  skills: () => (
    <>
      <P d="M120 18C100 11 70 11 48 18v40c22-7 52-7 72 0zM120 18c20-7 50-7 72 0v40c-22-7-52-7-72 0z" />
      <P d="M60 26l3 3 5-6M60 38l3 3 5-6M60 50l3 3 5-6" t="brand" i={2} />
      <P d="M76 26h30M76 38h30M76 50h24" t="soft" i={3} />
      <P d="M134 26h42M134 36h34M134 46h40" i={4} />
    </>
  ),
  // A recipe card with a {slot} to fill.
  templates: () => (
    <>
      <Box x={76} y={10} w={88} h={52} />
      <P d="M64 26l-9 10 9 10M176 26l9 10-9 10" t="brand" i={2} />
      <P d="M86 22h34" i={3} />
      <Box x={86} y={32} w={68} h={9} t="brand" i={4} />
      <P d="M86 52h48" t="soft" i={5} />
    </>
  ),
  // A film strip of snapshots with a playhead.
  timeline: () => (
    <>
      <Box x={16} y={18} w={208} h={38} />
      {[0, 1, 2, 3].map((k) => (
        <g key={k}>
          <Box x={28 + k * 48} y={28} w={36} h={18} t={k === 2 ? 'brand' : 'ink'} i={k + 1} />
          <P d={`M${30 + k * 48} 23h6M${44 + k * 48} 23h6M${30 + k * 48} 51h6M${44 + k * 48} 51h6`} t="soft" i={k} />
        </g>
      ))}
      <P d="M140 6h12l-6 7zM146 13v5" t="brand" i={6} />
    </>
  ),
  // A plug meeting a socket.
  setup: () => (
    <>
      <Box x={64} y={26} w={36} h={20} t="brand" />
      <P d="M64 36H44C30 36 34 54 18 54" t="brand" i={1} />
      <P d="M100 32h24M100 40h24" i={2} />
      <Box x={134} y={16} w={44} h={40} i={2} />
      <P d="M134 32h14M134 40h14" t="soft" i={3} />
      <P d="M188 26l7-8M192 36h10M188 46l7 8" t="ok" i={5} />
    </>
  ),
  // People around a shared key.
  workspace: () => (
    <>
      <C x={52} y={26} r={7} />
      <P d="M38 52c0-14 28-14 28 0" i={1} />
      <C x={188} y={26} r={7} i={1} />
      <P d="M174 52c0-14 28-14 28 0" i={2} />
      <C x={108} y={36} r={9} t="brand" i={2} />
      <P d="M117 36h28M134 36v7M142 36v5" t="brand" i={3} />
    </>
  ),
  // A heartbeat that ends steady.
  health: () => (
    <>
      <P d="M12 66h216" t="soft" />
      <P d="M12 40H70L78 34 84 40 94 14 104 60 112 34 118 40H218" t="brand" i={1} />
      <C x={226} y={40} r={4} t="ok" dot i={5} />
    </>
  ),
  // Three sliders, knobs at different places.
  settings: () => (
    <>
      {[0, 1, 2].map((k) => {
        const y = 16 + k * 20
        const x = [88, 156, 118][k]!
        return (
          <g key={k}>
            <P d={`M24 ${y}h192`} t="soft" i={k} />
            <P d={`M24 ${y}H${x}`} t="brand" i={k + 1} />
            <C x={x} y={y} r={6} dot i={k + 2} />
          </g>
        )
      })}
    </>
  ),
}

/** The band's drawing for a tour (240 x 72). The same drawing for every step of a tour. */
export function TourArt({ tourKey }: { tourKey: TourKey }) {
  const draw = ART[tourKey] ?? ART.now!
  return (
    <svg className="tour-art" viewBox="0 0 240 72" aria-hidden="true" focusable="false">
      {draw()}
    </svg>
  )
}

/** Welcome, step one: three agents on their own branches merge into one hub. About a second, then still. */
export function WelcomeHero({ still }: { still?: boolean }) {
  const ys = [29, 70, 111]
  return (
    <svg className={`tour-art tour-hero${still ? ' is-still' : ''}`} viewBox="0 0 280 140" aria-hidden="true" focusable="false">
      <circle className="halo" cx={218} cy={70} r={30} />
      {ys.map((y, k) => (
        <g key={y}>
          <path className="ln brand" pathLength={1} style={{ '--i': k * 2 } as CSSProperties} d={`M46 ${y}H${k === 1 ? 190 : `96C140 ${y} 140 70 190 70`}`} />
          <g className="agent" style={{ '--i': k } as CSSProperties}>
            <rect x={24} y={y - 11} width={22} height={22} rx={6} />
            <path d={['M30 ' + (y + 5) + 'l5-10 5 10M32 ' + (y + 2) + 'h6', 'M31 ' + (y - 5) + 'v10h8M31 ' + (y - 5) + 'h7', 'M30 ' + y + 'h12M36 ' + (y - 6) + 'v12'][k]} />
          </g>
        </g>
      ))}
      <g className="hub">
        <circle cx={208} cy={70} r={17} />
        <path d="M200 70h14M210 63l7 7-7 7" />
      </g>
    </svg>
  )
}

/** Small glyphs for the card's rows, drawn to match the art. */
export const Glyph = ({ kind }: { kind: 'why' | 'how' | 'wait' }) => (
  <svg className="tour-glyph" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
    {kind === 'why' && <path d="M4 3.5v9M4 8c0-3 8-1 8-4.5" />}
    {kind === 'why' && <><circle cx={4} cy={3} r={1.6} /><circle cx={4} cy={13} r={1.6} /><circle cx={12} cy={3} r={1.6} /></>}
    {kind === 'how' && <path d="M4 2.5l8 5.5-3.6 1 2.1 4.2-1.7.9-2.1-4.2L4 12.4z" />}
    {kind === 'wait' && <path d="M4.5 2h7M4.5 14h7M5 2c0 3.5 6 3.5 6 6s-6 2.5-6 6M11 2c0 3.5-6 3.5-6 6" />}
  </svg>
)
