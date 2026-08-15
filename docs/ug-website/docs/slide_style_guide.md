# Slide Style Guide: Modern Tech Minimalist

This document describes the visual language and UI components for a series of developer-focused presentation slides, based on the provided reference images.

## 1. General Aesthetic
- **Theme:** Clean, professional, and "premium" minimalist.
- **Layout:** Centered focus with generous whitespace (breathing room).
- **Background:** 
  - Primarily clean white or extremely light gray.
  - Subtle mesh gradients (soft blues/teals) are used to add depth without being distracting.
- **Corner Radius:** High (16px to 24px) for containers and windows, giving a modern "software" feel.

## 2. Color Palette
- **Primary Text:** Dark Charcoal (#1F2937) for main headings.
- **Secondary Text:** Slate Gray (#64748B) for descriptions and footers.
- **Accent Color:** Vibrant Teal/Cyan (#14B8A6 or #2DD4BF) used for highlighting key phrases and decorative lines.
- **Code Background:** Deep Midnight Blue/Black (#0F172A) to provide high contrast against the light slide background.

## 3. Typography
- **Font Family:** Modern Sans-serif (e.g., Inter, Roboto, or System Default).
- **Headings:** Bold and large. Use the accent color to highlight the most important 2-3 words in a sentence.
- **Hierarchy:** Clear distinction between the Tag/Label (top), Heading (middle), and Footnote/Context (bottom).

## 4. Key Components

### A. Terminal / Code Window
- **Container:** Dark, rounded rectangle with a subtle outer glow or soft shadow.
- **Window Controls:** Classic macOS "traffic light" buttons (Red, Yellow, Green) in the top-left corner.
- **Syntax Highlighting:** A balanced dark theme (like One Dark or Dracula) with high-legibility colors for code.
- **Title Bar:** Small, centered file name or description text in a muted color.

### B. Header Badge (Tag)
- **Shape:** Rounded pill/capsule.
- **Style:** Dark background with white/teal text.
- **Content:** Often includes a small icon (e.g., `>_` for terminal).

### C. Footer / Context Bar
- **Position:** Fixed at the bottom or floating in a rounded container at the base of the slide.
- **Content:** Small-font text providing additional context or "pro-tips".
- **Design:** Usually a light gray background to separate it from the main canvas.

## 5. Implementation Notes for AI
- Use CSS `flex` or `grid` for centering.
- Apply `backdrop-filter: blur()` if implementing the footer as a floating glassmorphism element.
- Use `box-shadow: 0 25px 50px -12px rgba(0, 0, 0, 0.25)` for the main code window to give it depth.
