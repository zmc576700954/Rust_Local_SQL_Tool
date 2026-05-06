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
          bg: '#0D1117',
          panel: '#161B22',
          border: '#30363D',
          text: '#C9D1D9',
          accent: '#3B82F6'
        }
      }
    },
  },
  plugins: [],
}