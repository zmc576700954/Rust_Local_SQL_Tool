import { type ButtonHTMLAttributes, forwardRef } from 'react'

type ButtonVariant = 'primary' | 'secondary' | 'toolbar' | 'danger'
type ButtonSize = 'sm' | 'md' | 'lg'

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant
  size?: ButtonSize
}

const variantStyles: Record<ButtonVariant, string> = {
  primary: 'bg-blue-600 hover:bg-blue-500 text-white font-medium shadow-sm',
  secondary: 'bg-dark-panel border border-dark-border hover:bg-dark-border text-dark-text hover:text-white shadow-sm',
  toolbar: 'bg-dark-surface hover:bg-dark-border border border-dark-border text-gray-300 hover:text-white',
  danger: 'bg-red-600 hover:bg-red-700 text-white font-medium',
}

const sizeStyles: Record<ButtonSize, string> = {
  sm: 'px-2 py-0.5 text-xs rounded',
  md: 'px-3 py-1.5 text-sm rounded',
  lg: 'px-4 py-2 text-sm rounded-md',
}

const baseStyles = 'inline-flex items-center justify-center gap-1.5 transition-all duration-150 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500/50 focus-visible:ring-offset-2 focus-visible:ring-offset-dark-bg disabled:opacity-50 disabled:cursor-not-allowed active:scale-[0.97]'

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(
  ({ variant = 'secondary', size = 'md', className = '', children, ...props }, ref) => {
    return (
      <button
        ref={ref}
        className={`${baseStyles} ${variantStyles[variant]} ${sizeStyles[size]} ${className}`}
        {...props}
      >
        {children}
      </button>
    )
  }
)

Button.displayName = 'Button'
