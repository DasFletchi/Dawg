// btw rote dateien heissen einfach nur von git untracked
// das package ding ist einfach nur die postadresse für gradle (den compiler)
// Wichtig: In build.gradle.kts steht, dass Gradle 'org.example.MainKt' starten soll. deswegen muss das da auch so sein
//wir setzen damit grade einfach nur die main scene wie in godot
package org.example

fun main(){
    var hello: String = "Hi Mom"
    var messa1 = "Dawg is so back"
    var messa2 = "67 rizller shall rise again."
    println(hello)
    println(messa1)
    println(messa2)
}