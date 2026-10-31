// Line illustrations for the tour band, drawn for the product: agents, lanes, merges, gates, timelines.
// Every drawing is data: a scene is a list of parts on a 400 x 100 frame, one part per thing a step explains. The step's
// part is drawn in brand violet and takes the camera (its own framing and zoom); the rest stay ink. Strokes draw in once
// (pathLength=1 + dash offset, see tour.css), then the subject part loops one gentle movement: a dot rides its path, or a halo breathes.
// With reduced motion the dot is left out and the halo is a still ring.
import type { CSSProperties, ReactNode } from 'react'
import type { TourKey } from './content.ts'

type Tone = 'ink' | 'soft' | 'brand' | 'bad' | 'ok'
interface K { t?: Tone; /** Stagger of the draw-in. */ i?: number; /** A filled node that pops instead of drawing. */ dot?: boolean; /** A soft tint inside the outline. */ f?: boolean }
const P = ({ d, t = 'ink', i, dot, f }: K & { d: string }) => (
  <path d={d} pathLength={1} className={`ln ${t}${dot ? ' dot' : ''}${f ? ' fill' : ''}`} style={i === undefined ? undefined : ({ '--i': i } as CSSProperties)} />
)
const C = ({ x, y, r = 4, ...k }: K & { x: number; y: number; r?: number }) => <P d={`M${x - r} ${y}a${r} ${r} 0 1 0 ${2 * r} 0a${r} ${r} 0 1 0 ${-2 * r} 0z`} {...k} />
const rr = (x: number, y: number, w: number, h: number, r: number) =>
  `M${x + r} ${y}h${w - 2 * r}a${r} ${r} 0 0 1 ${r} ${r}v${h - 2 * r}a${r} ${r} 0 0 1 ${-r} ${r}h${-(w - 2 * r)}a${r} ${r} 0 0 1 ${-r} ${-r}v${-(h - 2 * r)}a${r} ${r} 0 0 1 ${r} ${-r}z`
const Box = ({ x, y, w, h, r = 3, ...k }: K & { x: number; y: number; w: number; h: number; r?: number }) => <P d={rr(x, y, w, h, r)} {...k} />
/** A line of text. */
const L = ({ x, y, w, t = 'soft', i }: { x: number; y: number; w: number; t?: Tone; i?: number }) => <P d={`M${x} ${y}h${w}`} t={t} i={i} />
const Tick = ({ x, y, t = 'ok', i }: { x: number; y: number; t?: Tone; i?: number }) => <P d={`M${x - 3} ${y}l2.5 2.6 4.5-5`} t={t} i={i} />
/** An agent: a rounded face with two eyes. */
const Bot = ({ x, y, s = 16, ...k }: K & { x: number; y: number; s?: number }) => (
  <>
    <Box x={x} y={y} w={s} h={s} r={s / 3} {...k} />
    <P d={`M${x + s * 0.32} ${y + s * 0.42}v${s * 0.18}M${x + s * 0.68} ${y + s * 0.42}v${s * 0.18}`} t={k.t} i={k.i} />
  </>
)
const B = (h: boolean): Tone => (h ? 'brand' : 'ink')

/** `at` is where the camera looks; `ride` is a path a dot travels while the part is the subject (else a halo of radius `r` breathes). */
interface Part { at: [number, number]; ride?: string; r?: number; draw: (hot: boolean) => ReactNode }
const part = (at: [number, number], draw: Part['draw'], ride?: string, r?: number): Part => ({ at, draw, ride, r })

const TRACKS: [number, number, Tone][][] = [
  [[42, 70, 'ink'], [116, 100, 'brand'], [220, 60, 'soft']],
  [[42, 100, 'ink'], [146, 52, 'bad'], [202, 70, 'soft']],
  [[42, 40, 'ink'], [86, 140, 'brand']],
]

// ---- The welcome tour: one scene per step after the hero (steps 2 to 10). The subject is always the first part. ----
const WELCOME: Part[][] = [
  // Start with the sentence: three numbers in one line, an underline being read.
  [
    part([200, 44], () => (
      <>
        <Box x={44} y={26} w={34} h={18} r={9} t="brand" f />
        <L x={90} y={35} w={52} />
        <Box x={152} y={26} w={34} h={18} r={9} t="brand" f i={1} />
        <L x={198} y={35} w={44} i={1} />
        <Box x={254} y={26} w={34} h={18} r={9} t="brand" f i={2} />
        <L x={300} y={35} w={56} i={2} />
        <L x={44} y={62} w={312} t="brand" i={4} />
      </>
    ), 'M44 62H356'),
    part([200, 82], () => [0, 1, 2].map((k) => <Box key={k} x={44 + k * 78} y={76} w={66} h={12} r={6} i={5 + k} />)),
  ],
  // Needs you: three rows, one button each; a cursor heads for the first.
  [
    part([314, 24], () => (
      <>
        {[0, 1, 2].map((k) => {
          const y = 12 + k * 26
          return (
            <g key={k}>
              <Box x={56} y={y} w={288} h={20} r={7} t={k === 0 ? 'brand' : 'ink'} i={k} />
              <C x={72} y={y + 10} r={4.5} t={k === 1 ? 'bad' : 'soft'} dot={k === 1} i={k} />
              <L x={86} y={y + 7} w={96} t="ink" i={k} />
              <L x={86} y={y + 13} w={60} i={k} />
              <Box x={292} y={y + 4} w={42} h={12} r={6} t={k === 0 ? 'brand' : 'soft'} f={k === 0} i={k + 1} />
            </g>
          )
        })}
      </>
    ), 'M356 90C344 68 334 44 316 28'),
  ],
  // The track: three tasks, each in four stretches, an agent at the head, a finish flag.
  [
    part([160, 24], () => (
      <>
        <P d="M326 10v80" t="soft" />
        <P d="M332 12h14l-3 4 3 4h-14" t="ok" f i={5} />
        {[0, 1, 2].map((k) => {
          const y = 24 + k * 26
          const segs = TRACKS[k]!
          return (
            <g key={k}>
              <Bot x={16} y={y - 8} t={k === 0 ? 'brand' : 'ink'} i={k} />
              {segs.map(([x, w, t], j) => <Box key={j} x={x} y={y - 4.5} w={w} h={9} r={4.5} t={t} f={t !== 'soft' && j > 0} i={k + j + 1} />)}
            </g>
          )
        })}
      </>
    ), 'M120 24H210'),
  ],
  // Inbox: two agents, two plans that overlap, a message passing between.
  [
    part([200, 52], () => (
      <>
        <Bot x={22} y={40} s={24} t="brand" />
        <Bot x={354} y={40} s={24} i={1} />
        <C x={180} y={52} r={30} t="brand" i={1} />
        <C x={220} y={52} r={30} i={2} />
        <P d="M200 29.6A30 30 0 0 1 200 74.4A30 30 0 0 1 200 29.6z" t="brand" f i={3} />
        <P d="M48 52H148M352 52H252" t="soft" i={2} />
        <Box x={174} y={4} w={52} h={14} r={7} i={4} />
        <L x={186} y={11} w={28} i={5} />
      </>
    ), 'M48 52H352'),
  ],
  // History: a trail of records, the front one stamped, a magnifier checking it.
  [
    part([190, 46], () => (
      <>
        <P d="M46 10v80" t="soft" />
        {[20, 50, 80].map((y, k) => (
          <g key={y}>
            <C x={46} y={y} r={4} t="brand" dot i={k + 1} />
            <L x={56} y={y} w={52} i={k + 1} />
          </g>
        ))}
        <Box x={168} y={22} w={100} h={60} r={6} t="soft" i={1} />
        <Box x={156} y={16} w={100} h={60} r={6} t="soft" i={2} />
        <Box x={144} y={10} w={100} h={60} r={6} i={3} />
        <L x={156} y={26} w={64} t="ink" i={4} />
        <L x={156} y={36} w={48} i={4} />
        <C x={220} y={52} r={12} t="ok" f i={5} />
        <Tick x={220} y={52} i={6} />
        <C x={318} y={42} r={18} t="brand" i={4} />
        <P d="M331 55l16 16" t="brand" i={5} />
        <L x={306} y={42} w={24} i={6} />
      </>
    ), 'M154 26H236'),
  ],
  // Skills: a commit meets a gate; a check passes only the exact files it ran on.
  [
    part([170, 50], () => (
      <>
        <Box x={30} y={36} w={36} h={28} r={6} />
        <L x={38} y={46} w={20} t="ink" />
        <L x={38} y={54} w={14} />
        <P d="M66 50H140M200 50H252" t="soft" i={1} />
        <P d="M146 22v56M194 22v56M140 22h60" t="brand" i={2} />
        <C x={170} y={50} r={11} t="ok" f i={3} />
        <Tick x={170} y={50} i={4} />
        <Box x={254} y={38} w={74} h={24} r={12} t="ok" i={4} />
        <Tick x={272} y={50} i={5} />
        <L x={284} y={50} w={32} i={5} />
      </>
    ), 'M66 50H300'),
  ],
  // Setup: a computer plugged into the hub; ready once it proves it.
  [
    part([200, 52], () => (
      <>
        <Box x={34} y={22} w={70} h={60} r={6} />
        <C x={44} y={30} r={2} t="soft" dot />
        <C x={52} y={30} r={2} t="soft" dot />
        <L x={34} y={38} w={70} />
        <P d="M46 52l8 6-8 6M60 68h20" t="brand" i={2} />
        <P d="M104 52H244" t="brand" i={1} />
        <C x={268} y={52} r={24} t="brand" f i={2} />
        <P d="M256 52h24M274 44l8 8-8 8" i={3} />
        <Box x={308} y={40} w={72} h={24} r={12} t="ok" i={4} />
        <C x={322} y={52} r={4} t="ok" dot i={5} />
        <L x={332} y={52} w={36} i={5} />
      </>
    ), 'M104 52H244'),
  ],
  // Your repository: the one you are in, others behind it, and the menu that switches.
  [
    part([320, 50], () => (
      <>
        <Box x={92} y={6} w={170} h={64} r={8} t="soft" i={1} />
        <Box x={76} y={14} w={170} h={64} r={8} t="soft" i={2} />
        <Box x={60} y={22} w={170} h={64} r={8} i={3} />
        <C x={76} y={37} r={5} t="brand" f i={4} />
        <L x={88} y={37} w={70} t="ink" i={4} />
        <P d="M212 34l4 4 4-4" t="brand" i={4} />
        <P d="M76 54h120M76 66h84" i={5} />
        <Box x={270} y={20} w={110} h={62} r={8} t="brand" i={3} />
        {[34, 50, 66].map((y, k) => (
          <g key={y}>
            <C x={284} y={y} r={3} t={k === 0 ? 'brand' : 'soft'} dot={k === 0} i={k + 4} />
            <L x={294} y={y} w={[56, 44, 60][k]!} t="ink" i={k + 4} />
          </g>
        ))}
        <Tick x={366} y={34} t="brand" i={7} />
      </>
    ), 'M284 34V66'),
  ],
  // The ? button: a page with a spotlight, and the question mark that starts it.
  [
    part([326, 50], () => (
      <>
        <Box x={30} y={8} w={230} h={84} r={8} t="soft" />
        <P d="M30 24h230" t="soft" i={1} />
        <C x={240} y={16} r={5} t="brand" f i={1} />
        <Box x={56} y={40} w={104} h={36} r={6} t="brand" f i={2} />
        <L x={176} y={48} w={60} i={3} />
        <L x={176} y={60} w={44} i={3} />
        <P d="M296 50C270 50 200 52 166 58M174 52l-8 6 9 4" t="brand" i={3} />
        <C x={326} y={50} r={28} t="brand" f i={4} />
        <P d="M317 42c0-13 18-13 18 0 0 7-9 6-9 15" i={5} />
        <C x={326} y={68} r={1.8} dot i={6} />
      </>
    ), undefined, 36),
  ],
]

// ---- Page tours: one scene per page. Step k makes part k the subject, so the band follows the copy. ----
const PAGES: Record<string, Part[]> = {
  // The Dashboard in miniature: the sentence, the tabs, the Needs you rows, the track.
  home: [
    part([170, 14], (h) => (
      <>
        {[36, 138, 232].map((x, k) => <Box key={x} x={x} y={8} w={22} h={11} r={5.5} t={B(h)} f i={k} />)}
        <P d="M64 13.5h66M166 13.5h58M260 13.5h50" t="soft" i={1} />
      </>
    ), 'M36 25H312'),
    part([150, 36], (h) => [0, 1, 2, 3].map((k) => <Box key={k} x={36 + k * 66} y={30} w={58} h={12} r={6} t={k === 0 ? B(h) : 'soft'} f={k === 0} i={k} />), 'M42 36H262'),
    part([130, 58], (h) => (
      <>
        {[0, 1].map((k) => (
          <g key={k}>
            <Box x={36} y={48 + k * 15} w={206} h={12} r={4} t={B(h)} i={k} />
            <C x={46} y={54 + k * 15} r={3} t={k === 0 ? 'bad' : 'soft'} dot={k === 0} i={k} />
            <L x={56} y={54 + k * 15} w={80} i={k} />
            <Box x={206} y={50 + k * 15} w={30} h={8} r={4} t={B(h)} f={k === 0} i={k + 1} />
          </g>
        ))}
        <Box x={256} y={48} w={130} h={27} r={5} i={3} />
        <L x={266} y={58} w={60} i={3} />
        <L x={266} y={66} w={90} i={3} />
      </>
    ), 'M52 54H206'),
    part([200, 84], (h) => (
      <>
        <Box x={36} y={80} w={96} h={8} r={4} t={B(h)} f />
        <Box x={136} y={80} w={60} h={8} r={4} t="bad" f i={1} />
        <Box x={200} y={80} w={70} h={8} r={4} i={2} />
        <Box x={274} y={80} w={40} h={8} r={4} t="soft" i={3} />
        <P d="M364 74v20M364 74h12l-3 4 3 4h-12" t="ok" i={4} />
      </>
    ), 'M36 84H360'),
  ],
  // People, the agents' files, and two chips that are the same file.
  team: [
    part([135, 78], (h) => (
      <>
        {[70, 200, 330].map((x, k) => (
          <g key={x}>
            <C x={x} y={18} r={8} t={B(h)} i={k} />
            <P d={`M${x - 16} 46c0-16 32-16 32 0`} i={k + 1} />
            <Box x={x - 26} y={54} w={52} h={11} r={5.5} t="soft" f i={k + 1} />
            <Box x={x - 26} y={72} w={52} h={11} r={5.5} t={k < 2 ? 'bad' : 'soft'} f={k < 2} i={k + 2} />
          </g>
        ))}
        <P d="M96 77.5H174" t="bad" i={4} />
      </>
    ), 'M96 77.5H174'),
  ],
  // Two plans overlap; a prompt goes to an agent.
  inbox: [
    part([100, 50], (h) => (
      <>
        <Box x={20} y={16} w={100} h={52} r={8} />
        <Box x={80} y={32} w={100} h={52} r={8} t={B(h)} i={1} />
        <Box x={80} y={32} w={40} h={36} r={4} t="brand" f i={2} />
        <P d="M95 44h9l4 4v10H95z" t="ink" i={3} />
        <L x={30} y={28} w={44} i={2} />
        <L x={134} y={74} w={36} i={2} />
      </>
    )),
    part([270, 36], (h) => (
      <>
        <Box x={208} y={12} w={122} h={48} r={10} t={B(h)} />
        <P d="M228 60l-5 11 20-11" t={B(h)} i={1} />
        <L x={220} y={26} w={84} i={2} />
        <L x={220} y={36} w={64} i={2} />
        <Box x={276} y={44} w={44} h={10} r={5} t="brand" f i={3} />
        <P d="M330 36h12" t="soft" i={3} />
        <Bot x={344} y={24} s={28} t={B(h)} i={2} />
      </>
    )),
  ],
  // A map of the repository: blocks, the contested ones in coral, a pull request on one.
  now: [
    part([80, 34], (h) => (
      <>
        <Box x={20} y={10} w={120} h={46} r={4} t={B(h)} />
        <Box x={218} y={10} w={80} h={46} r={4} t="soft" i={1} />
        <Box x={302} y={10} w={84} h={46} r={4} t="soft" i={1} />
        <Box x={218} y={60} w={168} h={30} r={4} t="soft" i={2} />
        <C x={36} y={24} r={3} t="brand" dot i={3} />
        <L x={32} y={40} w={70} i={3} />
      </>
    )),
    part([130, 40], (h) => (
      <>
        <Box x={144} y={10} w={70} h={46} r={4} t="bad" f />
        <Box x={20} y={60} w={80} h={30} r={4} t="bad" f i={1} />
        {h && <Box x={140} y={6} w={78} h={54} r={6} t="brand" i={2} />}
        <C x={160} y={24} r={3} t="bad" dot i={2} />
        <C x={176} y={24} r={3} t="bad" dot i={2} />
      </>
    )),
    part([160, 75], (h) => (
      <>
        <Box x={104} y={60} w={110} h={30} r={4} t={B(h)} />
        <C x={122} y={68} r={3} t="brand" dot i={1} />
        <C x={122} y={82} r={3} t="brand" dot i={1} />
        <C x={136} y={68} r={3} t="brand" i={1} />
        <P d="M122 71v8M136 71c0 6-14 3-14 8" t="brand" i={2} />
        <L x={148} y={75} w={50} i={2} />
      </>
    ), 'M114 75H204'),
  ],
  // What was reported finished, then the five views of it.
  work: [
    part([100, 50], (h) => (
      <>
        {[14, 34, 54, 74].map((y, k) => (
          <g key={y}>
            <Box x={24} y={y} w={186} h={14} r={5} t={B(h)} i={k} />
            <C x={37} y={y + 7} r={4} t="ok" f i={k + 1} />
            <Tick x={37} y={y + 7} i={k + 2} />
            <L x={50} y={y + 7} w={[90, 70, 110, 60][k]!} i={k + 1} />
          </g>
        ))}
      </>
    ), 'M37 21V81'),
    part([308, 50], (h) => (
      <>
        {[0, 1, 2, 3, 4].map((k) => (
          <g key={k}>
            <C x={248 + k * 30} y={22} r={2.5} t={B(h)} dot i={k} />
            <Box x={236 + k * 30} y={32} w={24} h={36} r={4} t={B(h)} i={k} />
            <L x={241 + k * 30} y={42} w={14} i={k + 1} />
          </g>
        ))}
      </>
    )),
  ],
  // Since you last checked: a line of events with a flag; the log under it; an arrow back.
  activity: [
    part([300, 52], (h) => (
      <>
        <L x={16} y={52} w={368} />
        {Array.from({ length: 10 }, (_, k) => {
          const x = 36 + k * 36
          return <C key={k} x={x} y={52} r={4} t={x > 252 ? 'brand' : 'soft'} dot={x > 252} i={k % 5} />
        })}
        <P d="M252 36V68M252 36h14l-3 4 3 4h-14" t={B(h)} f i={5} />
      </>
    ), 'M252 52H384'),
    part([100, 84], (h) => (
      <>
        {[76, 84, 92].map((y, k) => (
          <g key={y}>
            <C x={34} y={y} r={1.8} t={B(h)} dot i={k} />
            <L x={44} y={y} w={[150, 200, 120][k]!} i={k} />
          </g>
        ))}
      </>
    )),
    part([200, 14], (h) => <P d="M380 32C300 4 100 4 28 32M28 32l9-1M28 32l3 8" t={B(h)} />, 'M380 32C300 4 100 4 28 32'),
  ],
  // Checks: a pipeline that ends in a check but no approval; runs; search; older and newer.
  workflows: [
    part([180, 52], (h) => (
      <>
        {[30, 110, 190].map((x, k) => <Box key={x} x={x} y={40} w={32} h={24} r={5} t={B(h)} i={k} />)}
        <P d="M62 52h48M142 52h48M222 52h34" t="soft" i={1} />
        <C x={278} y={52} r={12} t="ok" f i={3} />
        <Tick x={278} y={52} i={4} />
        <Box x={310} y={42} w={64} h={20} r={10} t="soft" i={4} />
        <L x={324} y={52} w={36} i={5} />
      </>
    ), 'M62 52H266'),
    part([110, 18], (h) => (
      <>
        <Box x={30} y={10} w={150} h={8} r={4} t={B(h)} f />
        <Box x={30} y={22} w={110} h={8} r={4} t="soft" i={1} />
        <C x={198} y={14} r={3} t="ok" dot i={2} />
        <C x={198} y={26} r={3} t="bad" dot i={2} />
      </>
    )),
    part([100, 83], (h) => (
      <>
        <Box x={30} y={76} w={150} h={14} r={7} t={B(h)} />
        <C x={42} y={83} r={3.5} t={B(h)} i={1} />
        <P d="M45 86l3 3" t={B(h)} i={1} />
        <L x={54} y={83} w={70} i={2} />
      </>
    )),
    part([330, 83], (h) => (
      <>
        <L x={300} y={83} w={60} />
        <P d="M290 77l-7 6 7 6M370 77l7 6-7 6" t={B(h)} i={1} />
      </>
    ), 'M300 83H360'),
  ],
  // A change record: one page; search; the latest check in plain labels; known gaps.
  'change-record': [
    part([200, 50], (h) => (
      <>
        <Box x={150} y={8} w={100} h={84} r={6} t={B(h)} />
        <Box x={184} y={4} w={32} h={8} r={3} t="brand" f i={1} />
        <L x={162} y={26} w={40} i={2} />
        <P d="M162 40h6M174 40h64" t="bad" i={3} />
        <P d="M162 52h6M165 49v6M174 52h66" t="ok" i={4} />
        <L x={162} y={64} w={50} i={5} />
        <L x={162} y={76} w={62} i={5} />
      </>
    ), 'M162 82H238'),
    part([75, 30], (h) => (
      <>
        <Box x={20} y={22} w={110} h={14} r={7} t={B(h)} />
        <C x={32} y={29} r={3.5} t={B(h)} i={1} />
        <P d="M35 32l3 3" t={B(h)} i={1} />
        <L x={44} y={29} w={60} i={2} />
        <Box x={20} y={44} w={110} h={10} r={4} i={2} />
        <Box x={20} y={58} w={110} h={10} r={4} i={3} />
      </>
    )),
    part([324, 38], (h) => (
      <>
        <Box x={274} y={14} w={100} h={12} r={6} t="ok" f />
        <Box x={274} y={32} w={100} h={12} r={6} t={B(h)} i={1} />
        <Box x={274} y={50} w={100} h={12} r={6} t="soft" i={2} />
      </>
    )),
    part([324, 78], (h) => (
      <>
        <Box x={274} y={70} w={100} h={16} r={8} t={B(h)} />
        <P d="M286 78h16M312 78h8M330 78h4" t={B(h)} i={1} />
      </>
    )),
  ],
  // After merge: a branch lands on main and main stays green; search; recent changes.
  landed: [
    part([150, 60], (h) => (
      <>
        <L x={16} y={44} w={214} />
        <C x={36} y={44} r={4} dot t="soft" />
        <C x={70} y={44} r={4} t="soft" i={1} />
        <P d="M70 44C86 44 90 72 110 72H190C210 72 214 44 230 44" t={B(h)} i={2} />
        {[130, 150, 170].map((x, k) => <C key={x} x={x} y={72} r={3.5} t="brand" dot i={k + 3} />)}
        <P d="M238 44h146" t="ok" i={5} />
        <C x={230} y={44} r={8} t="ok" f i={4} />
        <Tick x={230} y={44} i={5} />
        {[290, 340].map((x) => <C key={x} x={x} y={44} r={4} t="ok" dot i={6} />)}
      </>
    ), 'M70 44C86 44 90 72 110 72H190C210 72 214 44 230 44'),
    part([80, 14], (h) => (
      <>
        <Box x={20} y={8} w={120} h={13} r={6.5} t={B(h)} />
        <C x={31} y={14.5} r={3.2} t={B(h)} i={1} />
        <P d="M34 17l3 3" t={B(h)} i={1} />
        <L x={43} y={14.5} w={60} i={2} />
      </>
    )),
    part([280, 14], (h) => (
      <>
        <L x={200} y={14} w={154} />
        {Array.from({ length: 8 }, (_, k) => <C key={k} x={200 + k * 22} y={14} r={4} t={B(h)} dot={k === 7} i={k % 4} />)}
      </>
    ), 'M200 14H354'),
  ],
  // Skills: the book of workflows, the adopted ticks, what can stop an agent, plain steps, a command.
  skills: [
    part([200, 36], (h) => (
      <>
        <P d="M200 16C180 9 150 9 128 16v40c22-7 52-7 72 0zM200 16c20-7 50-7 72 0v40c-22-7-52-7-72 0z" t={B(h)} />
        <P d="M140 26h38M140 36h38M140 46h30" t="soft" i={2} />
        <P d="M222 26h40M222 36h32M222 46h38" i={3} />
      </>
    )),
    part([60, 32], (h) => (
      <>
        {[18, 32, 46].map((y, k) => (
          <g key={y}>
            <C x={26} y={y} r={5} t={B(h)} i={k} />
            <Tick x={26} y={y} t={B(h)} i={k + 1} />
            <L x={38} y={y} w={[60, 48, 54][k]!} i={k + 1} />
          </g>
        ))}
      </>
    )),
    part([340, 28], (h) => (
      <>
        <P d="M327 14h10l7 7v14l-7 7h-10l-7-7V21z" t={B(h)} f />
        <P d="M326 28h12" t={B(h)} i={1} />
        <P d="M358 18h10l7 7v10l-7 7h-10l-7-7V25z" t="soft" i={2} />
      </>
    )),
    part([110, 80], (h) => (
      <>
        {[0, 1, 2].map((k) => (
          <g key={k}>
            <C x={26 + k * 80} y={80} r={5} t={B(h)} dot={k === 0} i={k} />
            <L x={36 + k * 80} y={80} w={50} i={k + 1} />
          </g>
        ))}
      </>
    )),
    part([324, 78], (h) => (
      <>
        <Box x={262} y={66} w={124} h={24} r={6} t={B(h)} />
        <P d="M272 74l6 4-6 4" t="brand" i={1} />
        <L x={286} y={78} w={60} i={2} />
      </>
    )),
  ],
  // Templates: a recipe with a slot; the change it came from; how far a run got; where it runs.
  templates: [
    part([200, 36], (h) => (
      <>
        <Box x={130} y={10} w={140} h={52} r={8} t={B(h)} />
        <P d="M118 28l-9 10 9 10M282 28l9 10-9 10" t="brand" i={1} />
        <L x={144} y={22} w={60} t="ink" i={2} />
        <Box x={144} y={32} w={112} h={10} r={3} t="brand" f i={3} />
        <L x={144} y={52} w={70} i={4} />
      </>
    )),
    part([52, 27], (h) => (
      <>
        <Box x={20} y={16} w={64} h={22} r={5} t={B(h)} />
        <L x={28} y={25} w={32} i={1} />
        <L x={28} y={31} w={22} i={1} />
        <P d="M84 27C100 27 106 32 124 34M118 29l7 5-8 3" t={B(h)} i={2} />
      </>
    ), 'M84 27C100 27 106 32 124 34'),
    part([230, 80], (h) => (
      <>
        <P d="M135 80h40M185 80h40" t="ok" />
        <P d="M236 80h38" t="soft" i={1} />
        <C x={130} y={80} r={5} t="ok" dot i={1} />
        <C x={180} y={80} r={5} t="ok" dot i={2} />
        <C x={230} y={80} r={6.5} t={B(h)} i={3} />
        <C x={280} y={80} r={5} t="soft" i={4} />
      </>
    ), 'M130 80H230'),
    part([338, 40], (h) => (
      <>
        <Box x={306} y={14} w={64} h={38} r={4} t={B(h)} />
        <P d="M338 52v8M324 60h28" t={B(h)} i={1} />
        <L x={316} y={26} w={30} i={2} />
        <L x={316} y={36} w={44} i={2} />
        <Box x={330} y={66} w={44} h={20} r={3} i={3} />
        <P d="M324 90h56" i={3} />
      </>
    )),
  ],
  // Timeline: a film strip of saved snapshots; pins on the moments; two frames compared.
  timeline: [
    part([200, 48], (h) => (
      <>
        <Box x={16} y={30} w={368} h={36} r={6} t={B(h)} />
        {[0, 1, 2, 3, 4].map((k) => <Box key={k} x={26 + k * 70} y={38} w={56} h={20} r={3} t={k === 2 ? 'brand' : 'ink'} f={k === 2} i={k} />)}
      </>
    ), 'M26 48H374'),
    part([236, 14], (h) => [96, 236, 330].map((x, k) => (
      <g key={x}>
        <P d={`M${x} 28V15`} t="soft" i={k} />
        <C x={x} y={12} r={4} t={B(h)} dot i={k + 1} />
      </g>
    ))),
    part([200, 81], (h) => (
      <>
        <Box x={120} y={72} w={56} h={18} r={3} t={B(h)} />
        <Box x={224} y={72} w={56} h={18} r={3} i={1} />
        <P d="M182 81h36M188 77l-6 4 6 4M212 77l6 4-6 4" t="soft" i={2} />
        <P d="M128 79h24" t="bad" i={3} />
        <P d="M232 79h24" t="ok" i={3} />
      </>
    )),
  ],
  // Workspace: start, approve, land; then what you may do, who is in, the runs, the change to pick.
  workspace: [
    part([200, 22], (h) => (
      <>
        <P d="M92 22h96M212 22h96" t="soft" />
        {[80, 200, 320].map((x, k) => <C key={x} x={x} y={22} r={12} t={B(h)} i={k} />)}
        <P d="M77 16l8 6-8 6z" t={B(h)} f i={1} />
        <Tick x={200} y={22} t={B(h)} i={2} />
        <P d="M315 29V17M315 24c0-6 10-3 10-9" t={B(h)} i={3} />
      </>
    ), 'M80 22H320'),
    part([65, 72], (h) => (
      <>
        <P d="M65 52l22 8v14c0 10-10 16-22 20-12-4-22-10-22-20V60z" t={B(h)} f />
        <Tick x={65} y={74} t="brand" i={2} />
      </>
    )),
    part([164, 72], (h) => [136, 164, 192].map((x, k) => (
      <g key={x}>
        <C x={x} y={60} r={7} t={B(h)} i={k} />
        <P d={`M${x - 11} 88c0-14 22-14 22 0`} t={B(h)} i={k + 1} />
      </g>
    ))),
    part([266, 68], (h) => (
      <>
        {[56, 68, 80].map((y, k) => <Box key={y} x={226} y={y} w={[80, 56, 68][k]!} h={8} r={4} t={B(h)} f={k === 0} i={k} />)}
        <C x={316} y={60} r={3} t="ok" dot i={3} />
      </>
    )),
    part([350, 74], (h) => (
      <>
        {[60, 74, 88].map((y, k) => (
          <g key={y}>
            <C x={332} y={y} r={4} t={k === 0 ? B(h) : 'soft'} dot={k === 0} i={k} />
            <L x={342} y={y} w={[40, 34, 38][k]!} i={k} />
          </g>
        ))}
      </>
    )),
  ],
  // Setup: a computer proves itself to the hub; verified sessions; four steps; instructions to copy.
  setup: [
    part([115, 45], (h) => (
      <>
        <Box x={20} y={22} w={70} h={46} r={5} t={B(h)} />
        <P d="M30 38l6 5-6 5M42 50h14" t="brand" i={1} />
        <P d="M90 45H142" t="brand" i={1} />
        <C x={160} y={45} r={16} t="brand" f i={2} />
        <P d="M152 45h16M164 39l6 6-6 6" i={3} />
        <C x={184} y={29} r={3} t="ok" dot i={4} />
      </>
    ), 'M90 45H142'),
    part([300, 18], (h) => (
      <>
        {[0, 1, 2].map((k) => (
          <g key={k}>
            <Box x={210 + k * 60} y={10} w={52} h={16} r={8} t={B(h)} i={k} />
            <C x={222 + k * 60} y={18} r={3.5} t="ok" dot i={k + 1} />
            <L x={232 + k * 60} y={18} w={22} i={k + 1} />
          </g>
        ))}
      </>
    )),
    part([300, 52], (h) => (
      <>
        <P d="M230 52h40M282 52h40M334 52h40" t="soft" />
        {[0, 1, 2, 3].map((k) => <C key={k} x={224 + k * 52} y={52} r={6} t={B(h)} dot={k === 0} i={k} />)}
      </>
    ), 'M224 52H376'),
    part([276, 79], (h) => (
      <>
        <Box x={226} y={68} w={100} h={22} r={6} t={B(h)} />
        <L x={236} y={76} w={60} i={1} />
        <L x={236} y={83} w={44} i={1} />
        <Box x={346} y={70} w={16} h={16} r={3} t="brand" i={2} />
        <Box x={340} y={76} w={16} h={16} r={3} t="brand" f i={3} />
      </>
    )),
  ],
  // Health: a heartbeat that ends steady; how often the hook stepped in; how fast it was.
  health: [
    part([150, 28], (h) => (
      <>
        <P d="M16 28H130l8-6 6 6 10-22 10 38 8-16H360" t={B(h)} />
        <C x={372} y={28} r={4} t="ok" dot i={5} />
      </>
    ), 'M16 28H360'),
    part([100, 76], (h) => (
      <>
        <L x={16} y={92} w={180} />
        {[10, 18, 8, 26, 14, 6, 20, 12].map((v, k) => <Box key={k} x={22 + k * 21} y={92 - v - 4} w={12} h={v} r={2} t={k === 3 ? B(h) : 'soft'} f={k === 3} i={k % 4} />)}
      </>
    )),
    part([296, 76], (h) => (
      <>
        <P d="M250 90a46 46 0 0 1 92 0" t="soft" />
        <P d="M296 90L324 60" t={B(h)} i={1} />
        <C x={296} y={90} r={4} t={B(h)} dot i={2} />
      </>
    ), 'M250 90a46 46 0 0 1 92 0'),
  ],
  // Settings: sliders that are yours; theme; the walkthrough signpost; this computer; a key.
  settings: [
    part([190, 21], (h) => (
      <>
        {[110, 260, 190].map((x, k) => {
          const y = 9 + k * 12
          return (
            <g key={y}>
              <L x={20} y={y} w={360} i={k} />
              <L x={20} y={y} w={x - 20} t={B(h)} i={k + 1} />
              <C x={x} y={y} r={4} t={B(h)} dot i={k + 2} />
            </g>
          )
        })}
      </>
    ), 'M20 21H260'),
    part([60, 70], (h) => (
      <>
        <C x={60} y={70} r={18} t={B(h)} />
        <P d="M60 52a18 18 0 0 0 0 36z" t={B(h)} f i={1} />
      </>
    )),
    part([140, 70], (h) => (
      <>
        <P d="M140 52v38" t={B(h)} />
        <P d="M140 56h26l5 5-5 5h-26zM140 70h-24l-5 5 5 5h24z" t={B(h)} i={1} />
      </>
    )),
    part([230, 70], (h) => (
      <>
        <Box x={206} y={54} w={48} h={30} r={3} t={B(h)} />
        <P d="M198 88h64" t={B(h)} i={1} />
        <L x={214} y={64} w={20} i={2} />
        <L x={214} y={72} w={30} i={2} />
      </>
    )),
    part([340, 70], (h) => (
      <>
        <C x={318} y={70} r={10} t={B(h)} />
        <P d="M328 70h40M356 70v8M366 70v6" t={B(h)} i={1} />
      </>
    ), 'M328 70H368'),
  ],
}

const still = () => window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false
// The camera frames the subject: the first step of a page shows the whole scene, later ones lean in on their part.
const ZOOM = [1, 1.5, 1.25]
const clamp = (v: number, lo: number, hi: number) => Math.min(Math.max(v, lo), hi)

/** The band's drawing (400 x 100, aria-hidden). The welcome tour has its own scene per step; a page tour's parts take turns. */
export function TourArt({ tourKey, step }: { tourKey: TourKey; step: number }) {
  const welcome = tourKey === 'welcome'
  const parts = welcome ? WELCOME[(step - 1) % WELCOME.length]! : (PAGES[tourKey] ?? PAGES['home']!)
  const hot = welcome ? 0 : step % parts.length
  const z = welcome ? 1 : ZOOM[step % ZOOM.length]!
  const subject = parts[hot]!
  const [w, h] = [400 / z, 100 / z]
  const view = `${clamp(subject.at[0] - w / 2, 0, 400 - w).toFixed(1)} ${clamp(subject.at[1] - h / 2, 0, 100 - h).toFixed(1)} ${w.toFixed(1)} ${h.toFixed(1)}`
  const ride = subject.ride && !still()
  return (
    <svg className="tour-art" viewBox={view} style={{ '--sw': (1.6 / z).toFixed(2) } as CSSProperties} aria-hidden="true" focusable="false">
      {!ride && <circle className="breathe" cx={subject.at[0]} cy={subject.at[1]} r={subject.r ?? 20} />}
      {parts.map((p, k) => (
        <g key={k} className={k === hot ? undefined : 'rest'} style={{ '--b': k * 2 } as CSSProperties}>
          {p.draw(k === hot)}
        </g>
      ))}
      {ride && (
        <circle className="rider" r={3.6} opacity={0}>
          <animateMotion dur="5.6s" begin="1.4s" repeatCount="indefinite" path={subject.ride} keyPoints="0;1;1" keyTimes="0;0.72;1" calcMode="spline" keySplines=".5 0 .5 1;0 0 1 1" />
          <animate attributeName="opacity" values="0;1;1;0;0" keyTimes="0;0.06;0.66;0.74;1" dur="5.6s" begin="1.4s" repeatCount="indefinite" />
        </circle>
      )}
    </svg>
  )
}

/** Welcome, step one: three agents on their own branches merge into one hub, orbited by a ring. About a second, then still. */
export function WelcomeHero() {
  const ys = [29, 70, 111]
  return (
    <svg className="tour-art tour-hero" viewBox="0 0 400 140" aria-hidden="true" focusable="false">
      <g transform="translate(60 0)">
        <circle className="halo" cx={218} cy={70} r={30} />
        <circle className="ring" cx={208} cy={70} r={46} />
        <circle className="ring" cx={208} cy={70} r={74} />
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
