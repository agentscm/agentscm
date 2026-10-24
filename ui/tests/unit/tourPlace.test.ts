import { describe, expect, it } from 'vitest'
import { placeCard } from '../../src/tour/place.ts'

const card = { fullH: 460, h: 460 }
const desk = { w: 1280, h: 800 }

describe('placeCard', () => {
  it('centres a card with no target, wider for the welcome hero', () => {
    expect(placeCard(null, card, desk).style.width).toBe(400)
    expect(placeCard(null, { ...card, hero: true }, desk).style.width).toBe(480)
  })
  it('puts the full card below a small target', () => {
    const p = placeCard({ top: 100, left: 300, width: 200, height: 40 }, card, desk)
    expect(p.compact).toBe(false)
    expect(p.style.top).toBe(152)
  })
  it('goes compact for a target taller than the room the full card leaves, beside it when there is room', () => {
    const p = placeCard({ top: 60, left: 100, width: 300, height: 600 }, card, desk)
    expect(p.compact).toBe(true)
    expect(p.style.left).toBe(412)
  })
  it('docks bottom-right when a tall target leaves no side room', () => {
    const p = placeCard({ top: 20, left: 8, width: 1264, height: 760 }, card, desk)
    expect(p).toMatchObject({ compact: true, style: { right: 16, bottom: 16 } })
  })
  it('keeps the decision stable when the card shrinks to compact', () => {
    const r = { top: 100, left: 100, width: 400, height: 340 }
    expect(placeCard(r, { fullH: 460, h: 300 }, desk).compact).toBe(placeCard(r, card, desk).compact)
  })
  it('puts the phone sheet on the half the target leaves free, 45dvh at most', () => {
    const phone = { w: 375, h: 812 }
    const top = placeCard({ top: 60, left: 8, width: 360, height: 44 }, card, phone)
    expect(top).toMatchObject({ compact: true, sheet: true, style: { bottom: 12, maxHeight: '45dvh' } })
    const tight = placeCard({ top: 300, left: 8, width: 360, height: 180 }, card, phone) // 332px free below
    expect(tight.style.maxHeight).toBe('312px')
    const low = placeCard({ top: 640, left: 8, width: 360, height: 100 }, card, phone)
    expect(low.style.top).toBe(12)
  })
})
