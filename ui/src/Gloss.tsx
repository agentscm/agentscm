// API for other packages:
//   <Gloss term="contested">contested</Gloss>   the word with a dotted underline and its one-sentence tooltip
import type { ReactNode } from 'react'
import { glossText, type Term } from './glossary.ts'
import { Tip } from './ui.tsx'
import './plain.css'

/** A product word with a dotted underline and a focusable tooltip holding its gloss from glossary.ts. */
export function Gloss({ term, children }: { term: Term; children: ReactNode }) {
  return (
    <Tip text={glossText(term)} focusable className="gloss">
      {children}
    </Tip>
  )
}
