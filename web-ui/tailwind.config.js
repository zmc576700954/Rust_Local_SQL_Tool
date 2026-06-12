/** @type {import('tailwindcss').Config} */
export default {
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      colors: {
        dark: {
          canvas:   '#0a0c10',
          bg:       '#0D1117',
          panel:    '#161B22',
          surface:  '#21262d',
          border:   '#30363D',
          text:     '#C9D1D9',
          muted:    '#8B949E',
          accent:   '#3B82F6',
          success:  '#238636',
          danger:   '#DA3633',
        }
      }
    },
  },
  plugins: [],
}