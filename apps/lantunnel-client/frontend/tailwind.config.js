/** @type {import('tailwindcss').Config} */
// Semantic names only, resolved from the CSS variables in src/index.css. These
// are the same token names the web app uses, so a class can be read the same
// way on either surface.
export default {
  content: ['./index.html', './src/**/*.{js,ts,jsx,tsx}'],
  theme: {
    extend: {
      colors: {
        canvas: 'rgb(var(--color-canvas) / <alpha-value>)',
        surface: {
          DEFAULT: 'rgb(var(--color-surface) / <alpha-value>)',
          subdued: 'rgb(var(--color-surface-subdued) / <alpha-value>)',
        },
        border: 'rgb(var(--color-border) / <alpha-value>)',
        content: {
          primary: 'rgb(var(--color-text-primary) / <alpha-value>)',
          secondary: 'rgb(var(--color-text-secondary) / <alpha-value>)',
          muted: 'rgb(var(--color-text-muted) / <alpha-value>)',
          inverse: 'rgb(var(--color-text-inverse) / <alpha-value>)',
        },
        accent: {
          DEFAULT: 'rgb(var(--color-accent) / <alpha-value>)',
          soft: 'rgb(var(--color-accent-soft) / <alpha-value>)',
        },
        focus: 'rgb(var(--color-focus) / <alpha-value>)',
        status: {
          success: 'rgb(var(--color-success) / <alpha-value>)',
          warning: 'rgb(var(--color-warning) / <alpha-value>)',
          danger: 'rgb(var(--color-danger) / <alpha-value>)',
          info: 'rgb(var(--color-info) / <alpha-value>)',
        },
      },
    },
  },
  plugins: [],
}
