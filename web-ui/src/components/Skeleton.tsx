interface SkeletonProps {
  className?: string;
}

export function Skeleton({ className = '' }: SkeletonProps) {
  return (
    <div
      className={`animate-pulse bg-dark-panel border border-dark-border rounded-lg ${className}`}
    >
      <div className="w-full h-full bg-gradient-to-r from-transparent via-dark-surface to-transparent bg-[length:200%_100%] animate-shimmer" />
    </div>
  );
}

export function SkeletonLoader() {
  return (
    <div className="flex h-screen bg-dark-canvas text-gray-500 overflow-hidden">
      {/* Sidebar Skeleton */}
      <div className="w-64 border-r border-dark-border bg-dark-bg flex flex-col z-10">
        <div className="p-4 border-b border-dark-border flex items-center justify-between">
          <Skeleton className="h-6 w-32" />
          <Skeleton className="h-3 w-3 rounded-full" />
        </div>
        <div className="p-4 flex-1 space-y-4">
          <Skeleton className="h-4 w-20 mb-4" />
          {Array.from({ length: 6 }).map((_, i) => (
            <Skeleton key={i} className="h-8 w-full" />
          ))}
        </div>
        <div className="p-4 border-t border-dark-border space-y-3">
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </div>
      </div>

      {/* Main Content Skeleton */}
      <div className="flex-1 flex flex-col relative">
        <div className="h-10 border-b border-dark-border bg-dark-bg flex items-center px-4">
          <Skeleton className="h-6 w-24 rounded-t-lg" />
        </div>
        
        {/* Editor Area Skeleton */}
        <div className="flex-1 border-b border-dark-border p-6">
          <Skeleton className="h-4 w-3/4 mb-4" />
          <Skeleton className="h-4 w-1/2 mb-4" />
          <Skeleton className="h-4 w-2/3 mb-4" />
          
          <div className="absolute bottom-4 right-4 flex gap-3">
            <Skeleton className="h-10 w-10" />
            <Skeleton className="h-10 w-24" />
            <Skeleton className="h-10 w-20" />
          </div>
        </div>

        {/* Results Area Skeleton */}
        <div className="h-64 bg-dark-canvas flex flex-col relative">
          <div className="h-8 border-b border-dark-border bg-dark-bg flex items-center px-4">
            <Skeleton className="h-4 w-16" />
          </div>
          <div className="flex-1 p-4">
            <Skeleton className="h-full w-full" />
          </div>
        </div>
      </div>
    </div>
  );
}
