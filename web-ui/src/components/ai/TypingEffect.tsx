import { useState, useEffect } from 'react';

interface TypingEffectProps {
  text: string;
  speed?: number;
  className?: string;
  onComplete?: () => void;
}

export function TypingEffect({ text, speed = 30, className = '', onComplete }: TypingEffectProps) {
  const [displayedText, setDisplayedText] = useState('');

  useEffect(() => {
    setDisplayedText('');
    if (!text) return;

    let i = 0;
    const intervalId = setInterval(() => {
      setDisplayedText(text.slice(0, i + 1));
      i++;
      if (i >= text.length) {
        clearInterval(intervalId);
        if (onComplete) onComplete();
      }
    }, speed);

    return () => clearInterval(intervalId);
  }, [text, speed, onComplete]);

  return (
    <span className={className}>
      {displayedText}
      {displayedText.length < text.length && (
        <span className="inline-block w-[4px] h-[1em] bg-current ml-[2px] animate-pulse align-middle" />
      )}
    </span>
  );
}
